//! What to tell the user about Secure Boot signing, from what the firmware, the
//! installer and the key say. One place, so every frontend gives the same advice.

use crate::{Bootloader, SecureBootState, SigningCheck, SigningInfo, SigningScan};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// All as it should be.
    Done,
    /// Something to do next.
    Todo,
    /// Something is wrong, or about to be.
    Warn,
    /// Just so you know.
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advice {
    pub level: Level,
    pub text: String,
}

fn advice(level: Level, text: &str) -> Advice {
    Advice { level, text: text.to_owned() }
}

impl SigningInfo {
    pub fn secure_boot_on(&self) -> bool {
        self.secure_boot() == SecureBootState::Enabled
    }
}

/// What to do next, most pressing first. `scan` is `None` until the key has
/// been inspected, which needs authorization.
pub fn advise(loader: Bootloader, info: &SigningInfo, scan: Option<&SigningScan>) -> Vec<Advice> {
    if info.secure_boot() == SecureBootState::Unknown {
        return vec![advice(Level::Info, "This machine did not start through UEFI, so Secure Boot does not apply.")];
    }
    if loader != Bootloader::RBoot {
        return vec![advice(
            Level::Info,
            "Only r-boot's signing keys are managed here. The standard NixOS modules for this bootloader sign \
             nothing; lanzaboote (systemd-boot) or sbctl can. What the firmware says is shown above.",
        )];
    }
    if !info.manageable {
        return vec![advice(
            Level::Warn,
            "r-boot-cli is not installed, and it creates and checks the signing key. Add it to the system packages.",
        )];
    }

    let mut out = Vec::new();
    if !info.signs_bootloader {
        out.push(advice(
            Level::Todo,
            "The bootloader is not signed on rebuilds. Set boot.loader.r-boot.sign-bootloader = true in the NixOS \
             configuration, create a key here, and rebuild.",
        ));
    }
    let Some(scan) = scan else {
        out.push(advice(
            Level::Todo,
            "Check the key to see whether it exists, what it signed and whether the firmware knows it.",
        ));
        return out;
    };

    if !scan.key_present {
        out.push(if info.signs_bootloader {
            advice(
                Level::Warn,
                "The installer signs the bootloader, but there is no key: every rebuild fails until one is created.",
            )
        } else {
            advice(Level::Todo, "Create a signing key.")
        });
        return out;
    }

    let signed_here = scan.bootloader_signed() == SigningCheck::Yes && scan.signed_with_cert() == SigningCheck::Yes;
    if scan.bootloader_installed && !signed_here {
        out.push(advice(
            Level::Todo,
            "The installed bootloader is not signed with this key. Sign it: that runs the installer again.",
        ));
    }
    match (scan.enrolled(), info.secure_boot_on()) {
        (SigningCheck::No, true) => out.push(advice(
            Level::Warn,
            "Secure Boot is on, but the firmware does not know this key's certificate: the bootloader may not start. \
             Turn Secure Boot off in the firmware settings and enroll the certificate.",
        )),
        (SigningCheck::No, false) => out.push(advice(
            Level::Todo,
            "Enroll the certificate: restart into the r-boot menu and press e. The installer put it on the boot \
             partition as EFI/BOOT/db.der.",
        )),
        (SigningCheck::Yes, false) if signed_here => {
            out.push(advice(Level::Todo, "Everything is in place. Turn Secure Boot on in the firmware settings."))
        }
        (SigningCheck::Yes, true) if signed_here => out
            .push(advice(Level::Done, "Secure Boot is on and the bootloader is signed with a key the firmware knows.")),
        _ => {}
    }
    if out.is_empty() {
        out.push(advice(Level::Info, "Nothing to do."));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(secure_boot: SecureBootState, signs: bool) -> SigningInfo {
        SigningInfo {
            secure_boot: secure_boot as i32,
            signs_bootloader: signs,
            manageable: true,
            tool: "/bin/r-boot-cli".into(),
            ..Default::default()
        }
    }

    fn scan(signed: SigningCheck, enrolled: SigningCheck) -> SigningScan {
        SigningScan {
            key_present: true,
            cert_present: true,
            bootloader_installed: true,
            bootloader_signed: signed as i32,
            signed_with_cert: signed as i32,
            enrolled: enrolled as i32,
            ..Default::default()
        }
    }

    fn levels(advice: &[Advice]) -> Vec<Level> {
        advice.iter().map(|a| a.level).collect()
    }

    #[test]
    fn without_uefi_nothing_applies() {
        let advice = advise(Bootloader::RBoot, &SigningInfo::default(), None);
        assert_eq!(levels(&advice), vec![Level::Info]);
    }

    #[test]
    fn other_loaders_get_a_note_only() {
        let advice = advise(Bootloader::SystemdBoot, &info(SecureBootState::Disabled, false), None);
        assert_eq!(levels(&advice), vec![Level::Info]);
        assert!(advice[0].text.contains("lanzaboote"));
    }

    #[test]
    fn a_missing_cli_is_said() {
        let mut no_tool = info(SecureBootState::Disabled, true);
        no_tool.manageable = false;
        assert_eq!(levels(&advise(Bootloader::RBoot, &no_tool, None)), vec![Level::Warn]);
    }

    #[test]
    fn asks_for_a_check_first() {
        let advice = advise(Bootloader::RBoot, &info(SecureBootState::Disabled, true), None);
        assert_eq!(levels(&advice), vec![Level::Todo]);
    }

    #[test]
    fn signing_without_a_key_breaks_rebuilds() {
        let missing = SigningScan::default();
        let advice = advise(Bootloader::RBoot, &info(SecureBootState::Disabled, true), Some(&missing));
        assert_eq!(levels(&advice), vec![Level::Warn]);
        let advice = advise(Bootloader::RBoot, &info(SecureBootState::Disabled, false), Some(&missing));
        assert_eq!(levels(&advice), vec![Level::Todo, Level::Todo]);
    }

    #[test]
    fn walks_through_the_steps() {
        let off = info(SecureBootState::Disabled, true);
        let on = info(SecureBootState::Enabled, true);

        // Not signed with the key yet.
        let advice = advise(Bootloader::RBoot, &off, Some(&scan(SigningCheck::No, SigningCheck::Yes)));
        assert!(advice[0].text.contains("not signed with this key"));
        // Signed, but the firmware has to learn the certificate.
        let advice = advise(Bootloader::RBoot, &off, Some(&scan(SigningCheck::Yes, SigningCheck::No)));
        assert_eq!(levels(&advice), vec![Level::Todo]);
        assert!(advice[0].text.contains("press e"));
        // Enrolled: turn it on.
        let advice = advise(Bootloader::RBoot, &off, Some(&scan(SigningCheck::Yes, SigningCheck::Yes)));
        assert!(advice[0].text.contains("Turn Secure Boot on"));
        // On and all in place.
        let advice = advise(Bootloader::RBoot, &on, Some(&scan(SigningCheck::Yes, SigningCheck::Yes)));
        assert_eq!(levels(&advice), vec![Level::Done]);
        // On, but the firmware doesn't know the key.
        let advice = advise(Bootloader::RBoot, &on, Some(&scan(SigningCheck::Yes, SigningCheck::No)));
        assert_eq!(levels(&advice), vec![Level::Warn]);
    }
}
