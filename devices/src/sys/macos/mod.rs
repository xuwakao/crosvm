// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause

mod acpi;
pub(crate) mod serial_device;

pub(crate) use acpi::acpi_event_run;
pub(crate) use acpi::get_acpi_event_sock;
