// macOS iommu stub
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use anyhow::Result;
use cros_async::Executor;

use crate::virtio::iommu::State;

pub(in crate::virtio::iommu) async fn handle_command_tube(
    _state: &Rc<RefCell<State>>,
    _command_tube: cros_async::sys::platform::async_types::AsyncTube,
) -> Result<()> {
    futures::future::pending::<()>().await;
    Ok(())
}

pub(in crate::virtio::iommu) async fn handle_translate_request(
    _ex: &Executor,
    _state: &Rc<RefCell<State>>,
    _request_tube: Option<cros_async::sys::platform::async_types::AsyncTube>,
    _response_tubes: Option<BTreeMap<u32, cros_async::sys::platform::async_types::AsyncTube>>,
) -> Result<()> {
    futures::future::pending::<()>().await;
    Ok(())
}
