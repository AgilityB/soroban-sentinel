use sentinel_core::error::SentinelResult;
use sentinel_core::usecases::TTLReport;

pub fn render_junit(_report: &TTLReport) -> SentinelResult<String> {
    Ok(String::new())
}
