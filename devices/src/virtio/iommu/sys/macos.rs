// macOS iommu stub
use anyhow::Result;
use base::Tube;
use crate::virtio::iommu::IommuError;
pub(in crate::virtio::iommu) fn handle_command_tube(
    _tube: &Tube,
) -> Result<(), IommuError> {
    Ok(())
}
pub(in crate::virtio::iommu) fn handle_translate_request(
    _tube: &Tube,
) -> Result<(), IommuError> {
    Ok(())
}
