//! Secure Boot signing of the bootloader: what the firmware says, whether the
//! installer signs on every rebuild, and the key it signs with.
//!
//! Only r-boot is managed here. Its installer signs `BOOTX64.EFI` with a key
//! that `r-boot-cli sign-key` creates in a root-only directory, and the
//! firmware has to be taught that key's certificate once (r-boot's own menu
//! does that). The stock NixOS modules for systemd-boot and GRUB sign nothing,
//! so for them only the firmware's state is reported.
//!
//! What the firmware and the installer say needs no privileges and is part of
//! `BootInfo`. Looking at the key, and changing it, goes through `pkexec`.

// tonic::Status is large and every handler returns it.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};

use tonic::Status;

use crate::boot::{self, canonical, require_elevation, run_script, Installer, PROFILES_DIR};
use crate::nix_ops::find_in_path;
use crate::proto::{
    Bootloader, ManageSigningReply, ManageSigningRequest, SecureBootState, SigningAction, SigningCheck, SigningInfo,
    SigningScan,
};

const EFIVARS: &str = "/sys/firmware/efi/efivars";
const EFI_GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const EFI_IMAGE_SECURITY_GUID: &str = "d719b2cb-3d3a-4596-a3bc-dad00e67656f";

/// EFI_CERT_X509_GUID, as it is laid out in a signature list.
const X509_GUID: [u8; 16] =
    [0xa1, 0x59, 0xc0, 0xa5, 0xe4, 0x94, 0xa7, 0x4a, 0x87, 0xb5, 0xab, 0x15, 0x5c, 0x2b, 0xf0, 0x72];

const DEFAULT_KEY: &str = "/var/lib/r-boot/pki/db.key";
const DEFAULT_CERT: &str = "/var/lib/r-boot/pki/db.pem";

const MARK: &str = "@@nix-manager-sign:";

/// Reports the key, the certificate and the signature on the installed
/// bootloader. $1 PATH (pkexec wipes it), $2 key, $3 certificate, $4
/// r-boot-cli, $5 the partition's mount point.
const SCAN_SCRIPT: &str = r#"PATH="$1"; export PATH
key="$2"; cert="$3"; cli="$4"; esp="$5"
if [ -f "$key" ]; then echo "@@nix-manager-sign:key:present"; else echo "@@nix-manager-sign:key:missing"; fi
if [ -f "$cert" ]; then
  echo "@@nix-manager-sign:cert:present"
  if command -v openssl >/dev/null 2>&1; then
    echo "@@nix-manager-sign:info"
    openssl x509 -in "$cert" -noout -subject -enddate -fingerprint -sha256 2>&1
    echo "@@nix-manager-sign:der"
    openssl x509 -in "$cert" -outform DER 2>/dev/null | od -An -v -tx1 | tr -d ' \n'
    echo
  fi
else
  echo "@@nix-manager-sign:cert:missing"
fi
if [ -n "$cli" ]; then
  echo "@@nix-manager-sign:status"
  "$cli" -d "$esp" status 2>&1 || true
fi
echo "@@nix-manager-sign:end"
"#;

/// Changes the key, then signs by running the installer again. $1 PATH, $2
/// r-boot-cli, $3 the partition's mount point, $4 create|replace|remove|none,
/// $5 switch-to-configuration, $6 sign (0|1).
const MANAGE_SCRIPT: &str = r#"set -e
PATH="$1"; export PATH
cli="$2"; esp="$3"; action="$4"; switch="$5"; sign="$6"
case "$action" in
  create) "$cli" -d "$esp" sign-key create ;;
  replace) "$cli" -d "$esp" sign-key create --force ;;
  remove) "$cli" -d "$esp" sign-key remove --force ;;
esac
if [ "$sign" = 1 ]; then
  echo "Signing the bootloader: running the installer"
  "$switch" boot
fi
"#;

// ------------------------------------------------------------------- info --

/// The data of an EFI variable, without its four attribute bytes.
fn efi_variable(dir: &Path, name: &str, guid: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(dir.join(format!("{name}-{guid}"))).ok()?;
    bytes.get(4..).map(<[u8]>::to_vec)
}

/// A one-byte boolean variable such as `SecureBoot` or `SetupMode`.
fn efi_flag(dir: &Path, name: &str) -> Option<bool> {
    efi_variable(dir, name, EFI_GLOBAL_GUID)?.first().map(|byte| *byte != 0)
}

/// The X.509 certificates (DER) of a signature database: a sequence of
/// EFI_SIGNATURE_LISTs, each holding signatures of one type. Lists of hashes
/// are skipped. Truncated data ends the walk instead of failing it.
fn parse_signature_database(data: &[u8]) -> Vec<Vec<u8>> {
    let word = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let mut certs = Vec::new();
    let mut at = 0;
    while at + 28 <= data.len() {
        let (Some(list_size), Some(header_size), Some(signature_size)) = (word(at + 16), word(at + 20), word(at + 24))
        else {
            break;
        };
        if list_size < 28 || signature_size <= 16 || at + list_size > data.len() {
            break;
        }
        if data[at..at + 16] == X509_GUID {
            let mut signature = at + 28 + header_size;
            while signature + signature_size <= at + list_size {
                // Each signature starts with the GUID of whoever enrolled it.
                certs.push(data[signature + 16..signature + signature_size].to_vec());
                signature += signature_size;
            }
        }
        at += list_size;
    }
    certs
}

/// The certificates in the firmware's `db`, or `None` when it can't be read.
fn firmware_certificates(dir: &Path) -> Option<Vec<Vec<u8>>> {
    efi_variable(dir, "db", EFI_IMAGE_SECURITY_GUID).map(|data| parse_signature_database(&data))
}

fn unquote(value: &str) -> &str {
    value.trim().trim_end_matches('\\').trim().trim_matches(|c| c == '\'' || c == '"')
}

/// Where the installer reads the key and the certificate from.
fn installer_paths(text: &str) -> (String, String) {
    let key = text
        .lines()
        .find_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let at = words.iter().position(|word| *word == "--key")?;
            words.get(at + 1).map(|value| unquote(value).to_owned())
        })
        .filter(|value| value.starts_with('/'));
    let cert = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("CERT_FILE=").map(|value| unquote(value).to_owned()))
        .filter(|value| value.starts_with('/'));
    (key.unwrap_or_else(|| DEFAULT_KEY.to_owned()), cert.unwrap_or_else(|| DEFAULT_CERT.to_owned()))
}

pub fn info(loader: Bootloader, installer: Option<&Installer>) -> SigningInfo {
    let efivars = Path::new(EFIVARS);
    let text = installer.map_or("", |installer| installer.text.as_str());
    let (key_path, cert_path) = installer_paths(text);
    let tool = if loader == Bootloader::RBoot { find_in_path("r-boot-cli").unwrap_or_default() } else { String::new() };
    SigningInfo {
        secure_boot: match efi_flag(efivars, "SecureBoot") {
            Some(true) => SecureBootState::Enabled,
            Some(false) => SecureBootState::Disabled,
            None => SecureBootState::Unknown,
        } as i32,
        setup_mode: efi_flag(efivars, "SetupMode") == Some(true),
        signs_bootloader: text.contains("sbsign"),
        manageable: loader == Bootloader::RBoot && !tool.is_empty(),
        tool,
        key_path,
        cert_path,
        firmware_certs: firmware_certificates(efivars).map_or(0, |certs| certs.len() as u32),
    }
}

// ------------------------------------------------------------------- scan --

fn from_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
    let nibble = |digit: u8| (digit as char).to_digit(16).map(|value| value as u8);
    (0..digits.len() / 2).filter_map(|at| Some(nibble(digits[2 * at])? << 4 | nibble(digits[2 * at + 1])?)).collect()
}

fn check(value: Option<bool>) -> SigningCheck {
    match value {
        Some(true) => SigningCheck::Yes,
        Some(false) => SigningCheck::No,
        None => SigningCheck::Unknown,
    }
}

/// `yes` / `no (different certificate)` / anything else.
fn yes_no(value: &str) -> Option<bool> {
    let value = value.trim();
    if value.starts_with("yes") {
        Some(true)
    } else if value.starts_with("no") {
        Some(false)
    } else {
        None
    }
}

/// Reads the output of [`SCAN_SCRIPT`]. `firmware` is the firmware's
/// certificates, `None` when they couldn't be read. The lines of
/// `r-boot-cli status` are read as that tool prints them.
fn parse_scan(text: &str, firmware: Option<&[Vec<u8>]>) -> SigningScan {
    enum Section {
        None,
        Info,
        Der,
        Status,
    }

    let mut scan = SigningScan::default();
    let mut section = Section::None;
    let mut der = String::new();
    let mut status = String::new();
    let mut signed = None;
    let mut signed_with_cert = None;

    for line in text.lines() {
        if let Some(marker) = line.strip_prefix(MARK) {
            section = match marker {
                "key:present" => {
                    scan.key_present = true;
                    Section::None
                }
                "cert:present" => {
                    scan.cert_present = true;
                    Section::None
                }
                "info" => Section::Info,
                "der" => Section::Der,
                "status" => Section::Status,
                _ => Section::None,
            };
            continue;
        }
        match section {
            Section::Info => {
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim().to_ascii_lowercase();
                    if key == "subject" {
                        scan.cert_subject = value.trim().to_owned();
                    } else if key == "notafter" {
                        scan.cert_not_after = value.trim().to_owned();
                    } else if key.contains("fingerprint") {
                        scan.cert_fingerprint = value.trim().to_owned();
                    }
                }
            }
            Section::Der => der.push_str(line.trim()),
            Section::Status => {
                status.push_str(line);
                status.push('\n');
                let line = line.trim();
                if line.starts_with("bootloader: installed") {
                    scan.bootloader_installed = true;
                } else if let Some(value) = line.strip_prefix("signed:") {
                    signed = yes_no(value);
                } else if line.starts_with("signed with") {
                    signed_with_cert = line.rsplit_once("cert:").and_then(|(_, value)| yes_no(value));
                }
            }
            Section::None => {}
        }
    }

    scan.output = status.trim_end().to_owned();
    scan.bootloader_signed = check(signed) as i32;
    scan.signed_with_cert = check(signed_with_cert) as i32;
    let der = from_hex(&der);
    scan.enrolled = check(match (firmware, der.is_empty()) {
        (Some(certs), false) => Some(certs.contains(&der)),
        _ => None,
    }) as i32;
    scan
}

fn ensure_manageable(info: &SigningInfo) -> Result<(), Status> {
    if info.manageable {
        Ok(())
    } else if info.tool.is_empty() {
        Err(Status::failed_precondition(
            "Signing keys are managed through r-boot, and this system has no r-boot-cli installed.",
        ))
    } else {
        Err(Status::failed_precondition("Signing keys are only managed for r-boot."))
    }
}

pub async fn scan() -> Result<SigningScan, Status> {
    let info =
        tokio::task::spawn_blocking(boot::info).await.map_err(|e| Status::internal(format!("worker failed: {e}")))?;
    let signing = info.signing.clone().unwrap_or_default();
    ensure_manageable(&signing)?;

    // The key directory is root-only, and so usually is the partition.
    let elevate = !(info.listable && std::fs::metadata(&signing.cert_path).is_ok());
    if elevate {
        require_elevation()?;
    }
    let args = [
        std::env::var("PATH").unwrap_or_default(),
        signing.key_path.clone(),
        signing.cert_path.clone(),
        signing.tool.clone(),
        info.mount_point.clone(),
    ];
    let (ok, text) = run_script(elevate, SCAN_SCRIPT, &args).await?;
    if !ok {
        return Err(Status::failed_precondition(format!("Could not inspect the signing key: {}", text.trim())));
    }
    Ok(parse_scan(&text, firmware_certificates(Path::new(EFIVARS)).as_deref()))
}

// --------------------------------------------------------------- manage ---

/// What the request may do, given what the firmware and the installer say.
/// Nothing that would leave a bootloader the firmware refuses to start, or
/// that would make every rebuild fail, gets through.
fn check_request(signing: &SigningInfo, action: SigningAction, sign: bool) -> Result<(), Status> {
    ensure_manageable(signing)?;
    let secure_boot = SecureBootState::try_from(signing.secure_boot) == Ok(SecureBootState::Enabled);
    match action {
        SigningAction::None if !sign => Err(Status::invalid_argument("nothing to do")),
        SigningAction::ReplaceKey if secure_boot => Err(Status::failed_precondition(
            "Secure Boot is on: a new key would sign a bootloader the firmware does not trust yet, and it \
             would not start. Turn Secure Boot off in the firmware settings first, then enroll the new certificate.",
        )),
        SigningAction::RemoveKey if secure_boot => Err(Status::failed_precondition(
            "Secure Boot is on, and the firmware only starts a bootloader signed with this key. Turn Secure Boot \
             off first.",
        )),
        SigningAction::RemoveKey if signing.signs_bootloader => Err(Status::failed_precondition(
            "The installer is set to sign the bootloader (boot.loader.r-boot.sign-bootloader), so every rebuild \
             would fail without the key. Turn that off in the NixOS configuration and rebuild first.",
        )),
        _ => Ok(()),
    }
}

pub async fn manage(request: ManageSigningRequest) -> Result<ManageSigningReply, Status> {
    let info =
        tokio::task::spawn_blocking(boot::info).await.map_err(|e| Status::internal(format!("worker failed: {e}")))?;
    let signing = info.signing.clone().unwrap_or_default();
    let action = SigningAction::try_from(request.action)
        .map_err(|_| Status::invalid_argument(format!("unknown signing action {}", request.action)))?;
    check_request(&signing, action, request.sign_bootloader)?;
    require_elevation()?;

    let system: PathBuf = canonical(Path::new(PROFILES_DIR).join("system"))
        .ok_or_else(|| Status::failed_precondition("the system profile does not exist"))?;
    let switch = system.join("bin/switch-to-configuration");
    let word = match action {
        SigningAction::None => "none",
        SigningAction::CreateKey => "create",
        SigningAction::ReplaceKey => "replace",
        SigningAction::RemoveKey => "remove",
    };
    let args = [
        std::env::var("PATH").unwrap_or_default(),
        signing.tool,
        info.mount_point,
        word.to_owned(),
        switch.to_string_lossy().into_owned(),
        if request.sign_bootloader { "1" } else { "0" }.to_owned(),
    ];
    let (ok, output) = run_script(true, MANAGE_SCRIPT, &args).await?;
    if ok {
        Ok(ManageSigningReply { output })
    } else {
        Err(Status::failed_precondition(output.trim().to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALLER: &str = "set -e\nBOOT_EFI=\"/nix/store/x/EFI/BOOT/BOOTX64.EFI\"\nCERT_FILE=/var/lib/r-boot/pki/db.pem\n\
        /nix/store/y-sbsigntool-0.9.5/bin/sbsign \\\n  --key /var/lib/r-boot/pki/db.key \\\n  --cert \"$CERT_FILE\" \\\n  --output \"$SIGNED_EFI\" \\\n  \"$BOOT_EFI\"\n";

    #[test]
    fn reads_the_paths_from_the_installer() {
        assert_eq!(
            installer_paths(INSTALLER),
            ("/var/lib/r-boot/pki/db.key".to_owned(), "/var/lib/r-boot/pki/db.pem".to_owned())
        );
        let custom = "CERT_FILE='/etc/keys/c.pem'\nsbsign --key /etc/keys/k.key --cert \"$CERT_FILE\"\n";
        assert_eq!(installer_paths(custom), ("/etc/keys/k.key".to_owned(), "/etc/keys/c.pem".to_owned()));
        // A certificate given as text has no path; the default stands in.
        let inline = "CERT_FILE='-----BEGIN CERTIFICATE-----'\n";
        assert_eq!(installer_paths(inline).1, DEFAULT_CERT);
        assert_eq!(installer_paths("").0, DEFAULT_KEY);
    }

    fn signature_list(guid: [u8; 16], signatures: &[&[u8]]) -> Vec<u8> {
        let signature_size = 16 + signatures.iter().map(|s| s.len()).max().unwrap_or(0);
        let list_size = 28 + signature_size * signatures.len();
        let mut out = guid.to_vec();
        out.extend((list_size as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend((signature_size as u32).to_le_bytes());
        for signature in signatures {
            out.extend([7u8; 16]);
            out.extend(*signature);
            out.resize(out.len() + signature_size - 16 - signature.len(), 0);
        }
        out
    }

    #[test]
    fn reads_certificates_from_a_signature_database() {
        let mut db = signature_list(X509_GUID, &[b"cert-one", b"cert-two"]);
        // A list of hashes in between, which are not certificates.
        db.extend(signature_list([9u8; 16], &[b"0123456789abcdef"]));
        db.extend(signature_list(X509_GUID, &[b"cert-three"]));
        let certs = parse_signature_database(&db);
        assert_eq!(certs.len(), 3);
        assert_eq!(certs[0], b"cert-one");
        assert_eq!(certs[2], b"cert-three");
    }

    #[test]
    fn a_damaged_database_does_not_panic() {
        let db = signature_list(X509_GUID, &[b"cert-one"]);
        assert!(parse_signature_database(&db[..db.len() - 3]).is_empty());
        assert!(parse_signature_database(&[]).is_empty());
        assert!(parse_signature_database(&[0xff; 40]).is_empty());
    }

    #[test]
    fn reads_efi_variables() {
        let dir = std::env::temp_dir().join(format!("nix-manager-efivars-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("SecureBoot-{EFI_GLOBAL_GUID}")), [6, 0, 0, 0, 1]).unwrap();
        std::fs::write(dir.join(format!("SetupMode-{EFI_GLOBAL_GUID}")), [6, 0, 0, 0, 0]).unwrap();
        assert_eq!(efi_flag(&dir, "SecureBoot"), Some(true));
        assert_eq!(efi_flag(&dir, "SetupMode"), Some(false));
        assert_eq!(efi_flag(&dir, "Missing"), None);
        assert_eq!(firmware_certificates(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    const SCAN_OUTPUT: &str = "@@nix-manager-sign:key:present\n@@nix-manager-sign:cert:present\n@@nix-manager-sign:info\n\
        subject=CN = r-boot signing key\nnotAfter=Nov 21 10:00:00 2036 GMT\nsha256 Fingerprint=AA:BB:CC\n\
        @@nix-manager-sign:der\n0a0b0c\n\
        @@nix-manager-sign:status\npki-bundle:\n  private key: /var/lib/r-boot/pki/db.key (present)\n\nbootloader: installed at /boot/EFI/BOOT/BOOTX64.EFI\n  signed: yes\n  signed with /var/lib/r-boot/pki/db.pem cert: yes\n\
        @@nix-manager-sign:end\n";

    #[test]
    fn reads_the_scan() {
        let enrolled = vec![vec![1, 2, 3], vec![0x0a, 0x0b, 0x0c]];
        let scan = parse_scan(SCAN_OUTPUT, Some(&enrolled));
        assert!(scan.key_present && scan.cert_present && scan.bootloader_installed);
        assert_eq!(scan.cert_subject, "CN = r-boot signing key");
        assert_eq!(scan.cert_not_after, "Nov 21 10:00:00 2036 GMT");
        assert_eq!(scan.cert_fingerprint, "AA:BB:CC");
        assert_eq!(scan.bootloader_signed, SigningCheck::Yes as i32);
        assert_eq!(scan.signed_with_cert, SigningCheck::Yes as i32);
        assert_eq!(scan.enrolled, SigningCheck::Yes as i32);
        assert!(scan.output.contains("bootloader: installed"));

        // Not in the firmware, or the firmware can't be read.
        assert_eq!(parse_scan(SCAN_OUTPUT, Some(&[vec![1, 2, 3]])).enrolled, SigningCheck::No as i32);
        assert_eq!(parse_scan(SCAN_OUTPUT, None).enrolled, SigningCheck::Unknown as i32);
    }

    #[test]
    fn reads_a_scan_without_a_key() {
        let text = "@@nix-manager-sign:key:missing\n@@nix-manager-sign:cert:missing\n@@nix-manager-sign:status\nbootloader: not installed at /boot/EFI/BOOT/BOOTX64.EFI\n@@nix-manager-sign:end\n";
        let scan = parse_scan(text, Some(&[]));
        assert!(!scan.key_present && !scan.cert_present && !scan.bootloader_installed);
        assert_eq!(scan.bootloader_signed, SigningCheck::Unknown as i32);
        assert_eq!(scan.enrolled, SigningCheck::Unknown as i32);
    }

    #[test]
    fn a_different_certificate_is_told_apart() {
        let text = SCAN_OUTPUT.replace("cert: yes", "cert: no (different certificate)");
        assert_eq!(parse_scan(&text, None).signed_with_cert, SigningCheck::No as i32);
        let text = SCAN_OUTPUT.replace("cert: yes", "cert: unknown (no certificate there to compare against)");
        assert_eq!(parse_scan(&text, None).signed_with_cert, SigningCheck::Unknown as i32);
    }

    fn manageable(secure_boot: SecureBootState, signs: bool) -> SigningInfo {
        SigningInfo {
            manageable: true,
            tool: "/bin/r-boot-cli".into(),
            secure_boot: secure_boot as i32,
            signs_bootloader: signs,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_is_done_that_would_leave_an_unbootable_system() {
        let on = manageable(SecureBootState::Enabled, true);
        let off = manageable(SecureBootState::Disabled, false);
        assert!(check_request(&on, SigningAction::ReplaceKey, true).is_err());
        assert!(check_request(&on, SigningAction::RemoveKey, false).is_err());
        // Signing with the key as it is stays possible with Secure Boot on.
        assert!(check_request(&on, SigningAction::None, true).is_ok());
        assert!(check_request(&off, SigningAction::ReplaceKey, true).is_ok());
        assert!(check_request(&off, SigningAction::RemoveKey, false).is_ok());
        // Every rebuild would fail without the key the installer is set to sign with.
        assert!(check_request(&manageable(SecureBootState::Disabled, true), SigningAction::RemoveKey, false).is_err());
        assert!(check_request(&off, SigningAction::CreateKey, true).is_ok());
        assert!(check_request(&off, SigningAction::None, false).is_err());
    }

    #[test]
    fn other_loaders_are_not_managed() {
        let info = SigningInfo { manageable: false, ..Default::default() };
        assert!(check_request(&info, SigningAction::CreateKey, false).is_err());
        assert!(ensure_manageable(&SigningInfo { tool: "/x".into(), ..Default::default() }).is_err());
    }
}
