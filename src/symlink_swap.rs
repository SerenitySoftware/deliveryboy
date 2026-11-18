//! Rename a staged release link without following the live link.
use crate::{config::Target, remote::shell_quote};

pub struct SymlinkSwap {
    native_macos: bool,
}

impl SymlinkSwap {
    pub fn for_target(target: &Target) -> Self {
        Self::for_platform(target.is_local(), std::env::consts::OS)
    }

    // Explicit platform input lets every CI host check the macOS selection.
    pub fn for_platform(local: bool, os: &str) -> Self {
        Self {
            native_macos: local && os == "macos",
        }
    }

    /// BSD -h refuses to follow a link, but would nest the source in a real
    /// directory. Refuse non-links first. An outside writer can still race
    /// this check; unlike GNU -T, BSD mv cannot refuse that race itself.
    pub fn guard(&self, live: &str, sudo: &str) -> String {
        if !self.native_macos {
            return String::new();
        }
        let live = shell_quote(live);
        format!(
            "if {sudo}test -e {live} && ! {sudo}test -L {live}; then \
             echo 'live path is not a symlink; refusing to swap' >&2; exit 1; fi; "
        )
    }

    pub fn command(&self) -> &'static str {
        if self.native_macos {
            "/bin/mv -hf"
        } else {
            "mv -Tf"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_macos_uses_the_system_bsd_move() {
        for os in ["macos", "linux", "windows"] {
            for local in [false, true] {
                let swap = SymlinkSwap::for_platform(local, os);
                let native = local && os == "macos";
                assert_eq!(
                    swap.command(),
                    if native { "/bin/mv -hf" } else { "mv -Tf" }
                );
                assert_eq!(!swap.guard("/fixture/live", "sudo ").is_empty(), native);
                if native {
                    assert!(swap
                        .guard("/fixture/it's live", "sudo ")
                        .contains("sudo test -L '/fixture/it'\\''s live'"));
                }
            }
        }
    }
}
