// macOS arch platform stub
use devices::IommuDevType;

pub struct PlatformBusResources {
    pub dt_symbol: String,
    pub regions: Vec<(u64, u64)>,
    pub irqs: Vec<(u32, u32)>,
    pub iommus: Vec<(IommuDevType, Option<u32>, Vec<u32>)>,
    pub requires_power_domain: bool,
}
