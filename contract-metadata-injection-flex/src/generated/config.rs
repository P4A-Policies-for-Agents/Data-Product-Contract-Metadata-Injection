use serde::Deserialize;
#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(alias = "catalogId")]
    pub catalog_id: String,
    #[serde(
        alias = "cdgcLoginUrl",
        deserialize_with = "pdk::serde::deserialize_service"
    )]
    pub cdgc_login_url: pdk::hl::Service,
    #[serde(alias = "cdgcOrgPassword")]
    pub cdgc_org_password: String,
    #[serde(alias = "cdgcOrgUsername")]
    pub cdgc_org_username: String,
    #[serde(
        alias = "cdgcSearchUrl",
        deserialize_with = "pdk::serde::deserialize_service"
    )]
    pub cdgc_search_url: pdk::hl::Service,
    #[serde(alias = "distributed")]
    pub distributed: Option<bool>,
    #[serde(alias = "failOpenOnCdgcError")]
    pub fail_open_on_cdgc_error: Option<bool>,
    #[serde(alias = "flatFileId")]
    pub flat_file_id: String,
    #[serde(alias = "flatFileIdHeader")]
    pub flat_file_id_header: Option<String>,
    #[serde(alias = "headerOnMiss")]
    pub header_on_miss: Option<bool>,
    #[serde(alias = "refreshIntervalSeconds")]
    pub refresh_interval_seconds: Option<i64>,
    #[serde(alias = "sensitiveMarker")]
    pub sensitive_marker: Option<String>,
    #[serde(alias = "timeout")]
    pub timeout: Option<i64>,
}
#[pdk::hl::entrypoint_flex]
fn init(abi: &dyn pdk::flex_abi::api::FlexAbi) -> Result<(), anyhow::Error> {
    let config: Config = serde_json::from_slice(abi.get_configuration())
        .map_err(|err| {
            anyhow::anyhow!(
                "Failed to parse configuration '{}'. Cause: {}",
                String::from_utf8_lossy(abi.get_configuration()), err
            )
        })?;
    abi.service_create(config.cdgc_login_url)?;
    abi.service_create(config.cdgc_search_url)?;
    abi.setup()?;
    Ok(())
}
