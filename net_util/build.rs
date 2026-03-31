// Copyright 2020 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

static PREBUILTS_VERSION_FILENAME: &str = "prebuilts_version";
static SLIRP_LIB: &str = "libslirp.lib";
static SLIRP_DLL: &str = "libslirp-0.dll";
static GLIB_FILENAME: &str = "libglib-2.0.dll.a";

fn main() {
    // We (the Windows crosvm maintainers) submitted upstream patches to libslirp-sys so it doesn't
    // try to link directly on Windows. This is because linking on Windows tends to be specific
    // to the build system that invokes Cargo (e.g. the crosvm jCI scripts that also produce the
    // required libslirp DLL & lib). The integration here (win_slirp::main) is specific to crosvm's
    // build process.
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        let version = std::fs::read_to_string(PREBUILTS_VERSION_FILENAME)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        // TODO(b:242204245) build libslirp locally on windows from build.rs.
        let mut libs = vec![SLIRP_DLL, SLIRP_LIB];
        if std::env::var("CARGO_CFG_TARGET_ENV") == Ok("gnu".to_string()) {
            libs.push(GLIB_FILENAME);
        }
        prebuilts::download_prebuilts("libslirp", version, &libs).unwrap();
        for path in prebuilts::download_prebuilts("libslirp", version, &libs).unwrap() {
            println!(
                "cargo::rustc-link-search={}",
                path.parent().unwrap().display()
            );
        }
    }

    // For unix, libslirp-sys's build script will make the appropriate linking calls to pkg_config.

    // macOS: compile vmnet_helper.c, link vmnet.framework, and probe constants.
    if std::env::var("CARGO_CFG_TARGET_OS") == Ok("macos".to_string()) {
        println!("cargo:rustc-link-lib=framework=vmnet");

        // Compile the C helper for vmnet Objective-C block callbacks.
        // Do NOT use -fobjc-arc: ARC changes block lifetime semantics and
        // causes dispatch_semaphore + vmnet_start_interface to deadlock.
        // Manual memory management (MRC) matches the working standalone test.
        cc::Build::new()
            .file("src/sys/macos/vmnet_helper.c")
            .compile("vmnet_helper");

        // Probe vmnet.framework constants from the SDK headers so we don't
        // hardcode values that could change across macOS versions.
        // Falls back to known-good values (stable since macOS 10.10) if the
        // probe fails (e.g. cross-compilation, missing cc).
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let probe_out = format!("{}/vmnet_constants.rs", out_dir);

        let probe_result = (|| -> Result<String, String> {
            let probe_src = format!("{}/vmnet_probe.c", out_dir);
            let probe_bin = format!("{}/vmnet_probe", out_dir);

            std::fs::write(
                &probe_src,
                r#"
#include <vmnet/vmnet.h>
#include <stdio.h>
int main() {
    printf("pub const VMNET_SHARED_MODE: u64 = %llu;\n", (unsigned long long)VMNET_SHARED_MODE);
    printf("pub const VMNET_SUCCESS: u32 = %u;\n", (unsigned)VMNET_SUCCESS);
    return 0;
}
"#,
            )
            .map_err(|e| format!("write probe source: {}", e))?;

            let status = std::process::Command::new("cc")
                .args([&probe_src, "-o", &probe_bin, "-framework", "vmnet"])
                .status()
                .map_err(|e| format!("compile probe: {}", e))?;
            if !status.success() {
                return Err("probe compilation failed".into());
            }

            let output = std::process::Command::new(&probe_bin)
                .output()
                .map_err(|e| format!("run probe: {}", e))?;
            if !output.status.success() {
                return Err("probe execution failed".into());
            }

            let text = String::from_utf8(output.stdout)
                .map_err(|e| format!("probe output not UTF-8: {}", e))?;
            if !text.contains("pub const VMNET_SHARED_MODE")
                || !text.contains("pub const VMNET_SUCCESS")
            {
                return Err(format!("probe output missing constants: {}", text));
            }
            Ok(text)
        })();

        let constants = match probe_result {
            Ok(text) => text,
            Err(e) => {
                println!("cargo:warning=vmnet constants probe failed ({}), using fallback values", e);
                // Fallback: these values have been stable since macOS 10.10 (vmnet.framework introduction).
                "pub const VMNET_SHARED_MODE: u64 = 1001;\npub const VMNET_SUCCESS: u32 = 1000;\n".to_string()
            }
        };
        std::fs::write(&probe_out, constants)
            .expect("failed to write vmnet_constants.rs");
    }
}
