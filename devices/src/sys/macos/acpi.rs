// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS ACPI stub — no Netlink/ACPI event socket on macOS.

use std::sync::Arc;

use base::Event;
use sync::Mutex;

use crate::acpi::ACPIPMError;
use crate::acpi::GpeResource;
use crate::AcAdapter;
use crate::IrqLevelEvent;

/// macOS does not have Netlink sockets for ACPI events.
/// Returns None — ACPI host events are not available.
/// Uses Event as the socket type to satisfy AsRawDescriptor bounds.
pub(crate) fn get_acpi_event_sock() -> Result<Option<Event>, ACPIPMError> {
    Ok(None)
}

/// No-op on macOS — no ACPI event socket to process.
pub(crate) fn acpi_event_run(
    _sci_evt: &IrqLevelEvent,
    _acpi_event_sock: &Option<Event>,
    _gpe0: &Arc<Mutex<GpeResource>>,
    _ignored_gpe: &[u32],
    _ac_adapter: &Option<Arc<Mutex<AcAdapter>>>,
) {
    // No ACPI events on macOS.
}
