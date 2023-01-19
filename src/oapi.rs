use crate::args::Filter;
use crate::core::{self, Resources};
use crate::VERSION;
use aws_config::SdkConfig;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::{Credentials, Region};
use chrono::{DateTime, Utc};
use http::status::StatusCode;
use log::{info, trace, warn};
use outscale_api::apis::account_api::read_accounts;
use outscale_api::apis::catalog_api::read_catalog;
use outscale_api::apis::configuration::AWSv4Key;
use outscale_api::apis::configuration::Configuration;
use outscale_api::apis::configuration_file::{ConfigurationFile, ConfigurationFileError};
use outscale_api::apis::nat_service_api::read_nat_services;
use outscale_api::apis::public_ip_api::read_public_ips;
use outscale_api::apis::snapshot_api::read_snapshots;
use outscale_api::apis::subregion_api::read_subregions;
use outscale_api::apis::volume_api::read_volumes;
use outscale_api::apis::Error::ResponseError;
use outscale_api::models::{
    Account, CatalogEntry, FiltersNatService, FiltersPublicIp, FiltersSnapshot, FiltersVolume,
    FlexibleGpu, Image, NatService, PublicIp, ReadAccountsRequest, ReadAccountsResponse,
    ReadCatalogRequest, ReadCatalogResponse, ReadNatServicesRequest, ReadNatServicesResponse,
    ReadPublicIpsRequest, ReadPublicIpsResponse, ReadSnapshotsRequest, ReadSnapshotsResponse,
    ReadSubregionsRequest, ReadSubregionsResponse, ReadVolumesRequest, ReadVolumesResponse,
    Snapshot, Vm, VmType, Volume,
};
use rand::rngs::ThreadRng;
use rand::{thread_rng, Rng};
use secrecy::SecretString;
use std::collections::HashMap;
use std::convert::From;
use std::env;
use std::error;
use std::error::Error;
use std::thread::sleep;
use std::time::Duration;

use self::flexible_gpus::FlexibleGpuId;
use self::load_balancers::LoadbalancerId;
use self::oos::{BucketId, OosBucket};
use self::vms::VmId;
use self::vpn::VpnId;

static THROTTLING_MIN_WAIT_MS: u64 = 1000;
static THROTTLING_MAX_WAIT_MS: u64 = 10000;

type ImageId = String;
type VolumeId = String;
type SnapshotId = String;
type NatServiceId = String;

// This string correspond to pure internal forged identifier of a catalog entry.
// format!("{}/{}/{}", entry.service, entry.type, entry.operation);
// use catalog_entry() to ease process
type CatalogId = String;
type VmTypeName = String;
type PublicIpId = String;

mod flexible_gpus;
mod load_balancers;
mod oos;
mod vms;
mod vpn;

pub struct Input {
    config: Configuration,
    aws_config: SdkConfig,
    rng: ThreadRng,
    pub vms: HashMap<VmId, Vm>,
    pub vms_images: HashMap<ImageId, Image>,
    pub catalog: HashMap<CatalogId, CatalogEntry>,
    pub need_vm_types_fetch: bool,
    pub vm_types: HashMap<VmTypeName, VmType>,
    pub account: Option<Account>,
    pub region: Option<String>,
    pub nat_services: HashMap<NatServiceId, NatService>,
    pub volumes: HashMap<VolumeId, Volume>,
    pub snapshots: HashMap<SnapshotId, Snapshot>,
    pub fetch_date: Option<DateTime<Utc>>,
    pub public_ips: HashMap<PublicIpId, PublicIp>,
    pub filters: Option<Filter>,
    pub flexible_gpus: HashMap<FlexibleGpuId, FlexibleGpu>,
    pub load_balancers: Vec<LoadbalancerId>,
    pub vpns: Vec<VpnId>,
    pub buckets: HashMap<BucketId, OosBucket>,
}

impl Input {
    pub fn new(profile_name: String) -> Result<Input, Box<dyn error::Error>> {
        let (config, aws_config) = Input::get_config(profile_name)?;
        Ok(Input {
            config,
            aws_config,
            rng: thread_rng(),
            vms: HashMap::new(),
            vms_images: HashMap::new(),
            catalog: HashMap::new(),
            need_vm_types_fetch: false,
            vm_types: HashMap::new(),
            account: None,
            region: None,
            volumes: HashMap::<VolumeId, Volume>::new(),
            snapshots: HashMap::<SnapshotId, Snapshot>::new(),
            nat_services: HashMap::<NatServiceId, NatService>::new(),
            fetch_date: None,
            public_ips: HashMap::new(),
            filters: None,
            flexible_gpus: HashMap::new(),
            load_balancers: Vec::new(),
            vpns: Vec::new(),
            buckets: HashMap::new(),
        })
    }

    fn get_config(profile_name: String) -> Result<(Configuration, SdkConfig), Box<dyn Error>> {
        trace!("try to load api config from environment variables");
        let ak_env = env::var("OSC_ACCESS_KEY").ok();
        let sk_env = env::var("OSC_SECRET_KEY").ok();
        let region_env = env::var("OSC_REGION").ok();
        match (ak_env, sk_env, region_env) {
            (Some(access_key), Some(secret_key), Some(region)) => {
                let mut config = Configuration::new();
                config.base_path = format!("https://api.{}.outscale.com/api/v1", region);
                config.aws_v4_key = Some(AWSv4Key {
                    region: region.clone(),
                    access_key: access_key.clone(),
                    secret_key: SecretString::new(secret_key.clone()),
                    service: "oapi".to_string(),
                });
                config.user_agent = Some(format!("osc-cost/{VERSION}"));
                return Ok((
                    config,
                    Input::build_aws_config(access_key, secret_key, region),
                ));
            }
            (None, None, None) => {}
            (_, _, _) => {
                warn!("some credentials are set through environement variable but not all. OSC_ACCESS_KEY, OSC_SECRET_KEY and OSC_REGION are required to use this method.");
            }
        };

        trace!("try to load api config from configuration file");
        let config_file = ConfigurationFile::load_default()?;
        let mut config = config_file.configuration(&profile_name)?;
        config.user_agent = Some(format!("osc-cost/{VERSION}"));

        Ok((
            config,
            Input::build_aws_config_from_file(&config_file, profile_name)?,
        ))
    }

    fn build_aws_config(ak: String, sk: String, region: String) -> SdkConfig {
        let cred = Credentials::from_keys(ak, sk, None);
        // TODO: set Appname
        aws_config::SdkConfig::builder()
            .endpoint_url(format!("https://oos.{region}.outscale.com"))
            .region(Region::new(region))
            .credentials_provider(SharedCredentialsProvider::new(cred))
            .build()
    }

    fn build_aws_config_from_file<S: Into<String>>(
        config_file: &ConfigurationFile,
        profile_name: S,
    ) -> Result<SdkConfig, Box<dyn Error>> {
        let profile_name = profile_name.into();
        let profile = match config_file.0.get(&profile_name) {
            Some(profile) => profile.clone(),
            None => return Err(Box::new(ConfigurationFileError::ProfileNotFound)),
        };

        let region = profile.region.ok_or("No region for the profile")?;
        let access_key = profile.access_key.ok_or("No AK for the profile")?;
        let secret_key = profile.secret_key.ok_or("No SK for the profile")?;

        Ok(Input::build_aws_config(access_key, secret_key, region))
    }

    pub fn fetch(&mut self) -> Result<(), Box<dyn error::Error>> {
        self.fetch_date = Some(Utc::now());
        self.fetch_vms()?;
        self.fetch_vms_images()?;
        self.fetch_catalog()?;
        if self.need_vm_types_fetch {
            self.fetch_vm_types()?;
        }
        self.fetch_account()?;
        self.fetch_region()?;
        self.fetch_volumes()?;
        self.fetch_nat_services()?;
        self.fetch_public_ips()?;
        self.fetch_snapshots()?;
        self.fetch_flexible_gpus()?;
        self.fetch_load_balancers()?;
        self.fetch_vpns()?;
        self.fetch_buckets()?;
        Ok(())
    }

    fn fetch_catalog(&mut self) -> Result<(), Box<dyn error::Error>> {
        let result: ReadCatalogResponse = loop {
            let request = ReadCatalogRequest::new();
            let response = read_catalog(&self.config, Some(request));
            if Input::is_throttled(&response) {
                self.random_wait();
                continue;
            }
            break response?;
        };

        let catalog = match result.catalog {
            Some(catalog) => catalog,
            None => {
                warn!("no catalog provided");
                return Ok(());
            }
        };

        let catalog = match catalog.entries {
            Some(entries) => entries,
            None => {
                warn!("no catalog entries provided");
                return Ok(());
            }
        };
        for entry in catalog {
            let _type = match &entry._type {
                Some(t) => t.clone(),
                None => {
                    warn!("catalog entry as no type");
                    continue;
                }
            };
            let service = match &entry.service {
                Some(t) => t.clone(),
                None => {
                    warn!("catalog entry as no service");
                    continue;
                }
            };
            let operation = match &entry.operation {
                Some(t) => t.clone(),
                None => {
                    warn!("catalog entry as no operation");
                    continue;
                }
            };
            let entry_id = format!("{}/{}/{}", service, _type, operation);
            self.catalog.insert(entry_id, entry);
        }

        info!("fetched {} catalog entries", self.catalog.len());
        Ok(())
    }

    fn fetch_account(&mut self) -> Result<(), Box<dyn error::Error>> {
        let result: ReadAccountsResponse = loop {
            let request = ReadAccountsRequest::new();
            let response = read_accounts(&self.config, Some(request));
            if Input::is_throttled(&response) {
                self.random_wait();
                continue;
            }
            break response?;
        };

        let accounts = match result.accounts {
            None => {
                warn!("no account available");
                return Ok(());
            }
            Some(accounts) => accounts,
        };
        self.account = match accounts.first() {
            Some(account) => Some(account.clone()),
            None => {
                warn!("no account in account list");
                return Ok(());
            }
        };
        info!("fetched account details");
        Ok(())
    }

    fn fetch_region(&mut self) -> Result<(), Box<dyn error::Error>> {
        let result: ReadSubregionsResponse = loop {
            let request = ReadSubregionsRequest::new();
            let response = read_subregions(&self.config, Some(request));
            if Input::is_throttled(&response) {
                self.random_wait();
                continue;
            }
            break response?;
        };

        let subregions = match result.subregions {
            None => {
                warn!("no region available");
                return Ok(());
            }
            Some(subregions) => subregions,
        };
        self.region = match subregions.first() {
            Some(subregion) => subregion.region_name.clone(),
            None => {
                warn!("no subregion in region list");
                return Ok(());
            }
        };
        info!("fetched region details");
        Ok(())
    }

    fn fetch_volumes(&mut self) -> Result<(), Box<dyn error::Error>> {
        let result: ReadVolumesResponse = loop {
            let filter_volumes: FiltersVolume = match &self.filters {
                Some(filter) => FiltersVolume {
                    tag_keys: Some(filter.filter_tag_key.clone()),
                    tag_values: Some(filter.filter_tag_value.clone()),
                    tags: Some(filter.filter_tag.clone()),
                    ..Default::default()
                },
                None => FiltersVolume::new(),
            };
            let request = ReadVolumesRequest {
                filters: Some(Box::new(filter_volumes)),
                ..Default::default()
            };
            let response = read_volumes(&self.config, Some(request));

            if Input::is_throttled(&response) {
                self.random_wait();
                continue;
            }
            break response?;
        };

        let volumes = match result.volumes {
            None => {
                warn!("no volume available");
                return Ok(());
            }
            Some(volumes) => volumes,
        };
        for volume in volumes {
            let volume_id = volume.volume_id.clone().unwrap_or_else(|| String::from(""));
            self.volumes.insert(volume_id, volume);
        }
        info!("fetched {} volumes", self.volumes.len());
        Ok(())
    }

    fn fetch_nat_services(&mut self) -> Result<(), Box<dyn error::Error>> {
        let result: ReadNatServicesResponse = loop {
            let filters: FiltersNatService = match &self.filters {
                Some(filter) => FiltersNatService {
                    tag_keys: Some(filter.filter_tag_key.clone()),
                    tag_values: Some(filter.filter_tag_value.clone()),
                    tags: Some(filter.filter_tag.clone()),
                    ..Default::default()
                },
                None => FiltersNatService::new(),
            };
            let request = ReadNatServicesRequest {
                filters: Some(Box::new(filters)),
                ..Default::default()
            };
            let response = read_nat_services(&self.config, Some(request));
            if Input::is_throttled(&response) {
                self.random_wait();
                continue;
            }
            break response?;
        };
        let nat_services = match result.nat_services {
            None => {
                warn!("no nat_service available!");
                return Ok(());
            }
            Some(nat_services) => nat_services,
        };

        for nat_service in nat_services {
            let nat_service_id = nat_service
                .nat_service_id
                .clone()
                .unwrap_or_else(|| String::from(""));
            self.nat_services.insert(nat_service_id, nat_service);
        }
        info!("fetched {} nat_service", self.nat_services.len());
        Ok(())
    }
    fn fetch_public_ips(&mut self) -> Result<(), Box<dyn error::Error>> {
        let filters: FiltersPublicIp = match &self.filters {
            Some(filter) => FiltersPublicIp {
                tag_keys: Some(filter.filter_tag_key.clone()),
                tag_values: Some(filter.filter_tag_value.clone()),
                tags: Some(filter.filter_tag.clone()),
                ..Default::default()
            },
            None => FiltersPublicIp::new(),
        };
        let request = ReadPublicIpsRequest {
            filters: Some(Box::new(filters)),
            ..Default::default()
        };
        let result: ReadPublicIpsResponse = loop {
            let response = read_public_ips(&self.config, Some(request.clone()));
            if Input::is_throttled(&response) {
                self.random_wait();
                continue;
            }
            break response?;
        };
        let public_ips = match result.public_ips {
            None => {
                warn!("no public ip list provided");
                return Ok(());
            }
            Some(ips) => ips,
        };
        for public_ip in public_ips {
            let public_ip_id = match &public_ip.public_ip_id {
                Some(id) => id.clone(),
                None => {
                    warn!("public ip has no id");
                    continue;
                }
            };
            self.public_ips.insert(public_ip_id, public_ip);
        }

        info!("info: fetched {} public ips", self.public_ips.len());
        Ok(())
    }

    fn fetch_snapshots(&mut self) -> Result<(), Box<dyn error::Error>> {
        let account_id = match self.account_id() {
            None => {
                warn!("warning: no account_id available... skipping");
                return Ok(());
            }
            Some(account_id) => account_id,
        };
        let filters: FiltersSnapshot = match &self.filters {
            Some(filter) => FiltersSnapshot {
                account_ids: Some(vec![account_id]),
                tag_keys: Some(filter.filter_tag_key.clone()),
                tag_values: Some(filter.filter_tag_value.clone()),
                tags: Some(filter.filter_tag.clone()),
                ..Default::default()
            },
            None => FiltersSnapshot {
                account_ids: Some(vec![account_id]),
                ..Default::default()
            },
        };
        let request = ReadSnapshotsRequest {
            filters: Some(Box::new(filters)),
            ..Default::default()
        };
        let result: ReadSnapshotsResponse = loop {
            let response = read_snapshots(&self.config, Some(request.clone()));
            if Input::is_throttled(&response) {
                self.random_wait();
                continue;
            }
            break response?;
        };

        let snapshots = match result.snapshots {
            None => {
                warn!("warning: no snapshot available");
                return Ok(());
            }
            Some(snapshots) => snapshots,
        };
        for snapshot in snapshots {
            let snapshot_id = snapshot
                .snapshot_id
                .clone()
                .unwrap_or_else(|| String::from(""));
            self.snapshots.insert(snapshot_id, snapshot);
        }
        warn!("info: fetched {} snapshots", self.snapshots.len());
        Ok(())
    }

    fn random_wait(&mut self) {
        let wait_time_ms = self
            .rng
            .gen_range(THROTTLING_MIN_WAIT_MS..THROTTLING_MAX_WAIT_MS);
        info!("call throttled, waiting for {}ms", wait_time_ms);
        sleep(Duration::from_millis(wait_time_ms));
    }

    fn is_throttled<S, T>(result: &Result<S, outscale_api::apis::Error<T>>) -> bool {
        match result {
            Ok(_) => false,
            Err(error) => match error {
                ResponseError(resp) => matches!(
                    resp.status,
                    StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS
                ),
                _ => false,
            },
        }
    }

    fn account_id(&self) -> Option<String> {
        match &self.account {
            Some(account) => account.account_id.clone(),
            None => None,
        }
    }

    fn catalog_entry<S: Into<String>>(&self, service: S, type_: S, operation: S) -> Option<f32> {
        let entry_id = format!("{}/{}/{}", service.into(), type_.into(), operation.into());
        match self.catalog.get(&entry_id) {
            Some(entry) => match entry.unit_price {
                Some(price) => Some(price),
                None => {
                    warn!("cannot find price for {}", entry_id);
                    None
                }
            },
            None => {
                warn!("cannot find catalog entry for {}", entry_id);
                None
            }
        }
    }

    fn fill_resource_volume(&self, resources: &mut Resources) {
        for (volume_id, volume) in &self.volumes {
            let specs = match VolumeSpecs::new(volume, self) {
                Some(s) => s,
                None => continue,
            };
            let core_volume = core::Volume {
                osc_cost_version: Some(String::from(VERSION)),
                account_id: self.account_id(),
                read_date_rfc3339: self.fetch_date.map(|date| date.to_rfc3339()),
                region: self.region.clone(),
                resource_id: Some(volume_id.clone()),
                price_per_hour: None,
                price_per_month: None,
                volume_type: Some(specs.volume_type.clone()),
                volume_iops: Some(specs.iops),
                volume_size: Some(specs.size),
                price_gb_per_month: specs.price_gb_per_month,
                price_iops_per_month: specs.price_iops_per_month,
            };
            resources
                .resources
                .push(core::Resource::Volume(core_volume));
        }
    }
    fn fill_resource_nat_service(&self, resources: &mut Resources) {
        for (nat_service_id, nat_service) in &self.nat_services {
            let price_product_per_nat_service_per_hour =
                self.catalog_entry("TinaOS-FCU", "NatGatewayUsage", "CreateNatGateway");
            let Some(nat_service_id) = &nat_service.nat_service_id else {
                warn!("cannot get nat_service_id content for {}", nat_service_id );
                continue;
            };
            let core_nat_service = core::NatServices {
                osc_cost_version: Some(String::from(VERSION)),
                account_id: self.account_id(),
                read_date_rfc3339: self.fetch_date.map(|date| date.to_rfc3339()),
                region: self.region.clone(),
                resource_id: Some(nat_service_id.clone()),
                price_per_hour: None,
                price_per_month: None,
                price_product_per_nat_service_per_hour,
            };
            resources
                .resources
                .push(core::Resource::NatServices(core_nat_service));
        }
    }

    fn fill_resource_public_ip(&self, resources: &mut Resources) {
        for (public_ip_id, public_ip) in &self.public_ips {
            let mut price_non_attached: Option<f32> = None;
            let mut price_first_ip: Option<f32> = None;
            let mut price_next_ips: Option<f32> = None;
            let Some(public_ip_str) = &public_ip.public_ip else {
                warn!("cannot get public ip content for {}", public_ip_id);
                continue;
            };

            match &public_ip.vm_id {
                None => match self.catalog_entry(
                    "TinaOS-FCU",
                    "ElasticIP:IdleAddress",
                    "AssociateAddressVPC",
                ) {
                    Some(price) => price_non_attached = Some(price),
                    None => continue,
                },
                Some(vm_id) => match self.vms.get(vm_id) {
                    Some(vm) => match &vm.public_ip {
                        Some(vm_public_ip) => match *vm_public_ip == *public_ip_str {
                            // First Public IP is free
                            true => price_first_ip = Some(0_f32),
                            // Additional Public IP cost
                            false => {
                                price_next_ips = match self.catalog_entry(
                                    "TinaOS-FCU",
                                    "ElasticIP:AdditionalAddress",
                                    "AssociateAddressVPC",
                                ) {
                                    Some(price) => Some(price),
                                    None => continue,
                                }
                            }
                        },
                        None => {
                            warn!(
                                "vm {} does not seem to have any ip, should at least have {}",
                                vm_id, public_ip_str
                            );
                            continue;
                        }
                    },
                    None => {
                        warn!(
                            "cannot find vm id {} for public ip {} ({})",
                            vm_id, public_ip_str, public_ip_id
                        );
                        continue;
                    }
                },
            };
            let core_public_ip = core::PublicIp {
                osc_cost_version: Some(String::from(VERSION)),
                account_id: self.account_id(),
                read_date_rfc3339: self.fetch_date.map(|date| date.to_rfc3339()),
                region: self.region.clone(),
                resource_id: Some(public_ip_id.clone()),
                price_per_hour: None,
                price_per_month: None,
                price_non_attached,
                price_first_ip,
                price_next_ips,
            };
            resources
                .resources
                .push(core::Resource::PublicIp(core_public_ip));
        }
    }

    fn fill_resource_snapshot(&self, resources: &mut Resources) {
        let Some(price_gb_per_month) = self.catalog_entry("TinaOS-FCU", "Snapshot:Usage", "Snapshot") else {
            warn!("gib price is not defined for snapshot");
            return
        };
        for (snapshot_id, snapshot) in &self.snapshots {
            let core_snapshot = core::Snapshot {
                osc_cost_version: Some(String::from(VERSION)),
                account_id: self.account_id(),
                read_date_rfc3339: self.fetch_date.map(|date| date.to_rfc3339()),
                region: self.region.clone(),
                resource_id: Some(snapshot_id.clone()),
                price_per_hour: None,
                price_per_month: None,
                volume_size_gib: snapshot.volume_size,
                price_gb_per_month,
            };
            resources
                .resources
                .push(core::Resource::Snapshot(core_snapshot));
        }
    }
}

struct VolumeSpecs {
    volume_type: String,
    size: i32,
    iops: i32,
    price_gb_per_month: f32,
    price_iops_per_month: f32,
}

impl VolumeSpecs {
    fn new(volume: &Volume, input: &Input) -> Option<Self> {
        let volume_type = match &volume.volume_type {
            Some(volume_type) => volume_type,
            None => {
                warn!("warning: cannot get volume type in volume details");
                return None;
            }
        };

        let iops = volume.iops.unwrap_or_else(|| {
            if volume_type == "io1" {
                warn!("cannot get iops in volume details");
            }
            0
        });

        let size = match &volume.size {
            Some(size) => *size,
            None => {
                warn!("cannot get size in volume details");
                return None;
            }
        };
        let out = VolumeSpecs {
            volume_type: volume_type.clone(),
            iops,
            size,
            price_gb_per_month: 0_f32,
            price_iops_per_month: 0_f32,
        };

        out.parse_volume_prices(input)
    }

    fn parse_volume_prices(mut self, input: &Input) -> Option<VolumeSpecs> {
        let price_gb_per_month = input.catalog_entry(
            "TinaOS-FCU",
            &format!("BSU:VolumeUsage:{}", self.volume_type),
            "CreateVolume",
        )?;
        if self.volume_type == "io1" {
            self.price_iops_per_month = input.catalog_entry(
                "TinaOS-FCU",
                &format!("BSU:VolumeIOPS:{}", self.volume_type),
                "CreateVolume",
            )?;
        }
        self.price_gb_per_month = price_gb_per_month;
        Some(self)
    }
}

impl From<Input> for core::Resources {
    fn from(input: Input) -> Self {
        let mut resources = core::Resources {
            resources: Vec::new(),
        };
        input.fill_resource_vm(&mut resources);
        input.fill_resource_volume(&mut resources);
        input.fill_resource_public_ip(&mut resources);
        input.fill_resource_snapshot(&mut resources);
        input.fill_resource_nat_service(&mut resources);
        input.fill_resource_flexible_gpus(&mut resources);
        input.fill_resource_load_balancers(&mut resources);
        input.fill_resource_vpns(&mut resources);
        input.fill_resource_oos(&mut resources);
        resources
    }
}
