//! macOS default-terminal integration.
//!
//! LaunchServices has no system-wide "default terminal" switch. Instead it
//! remembers a handler per document type and URL scheme, which is exactly the
//! narrow promise tty7 can make: folders, runnable local files, SSH links, and
//! man-page links.

use std::path::PathBuf;

pub const BUNDLE_ID: &str = "com.github.tty7";
const URL_SCHEMES: &[&str] = &["ssh", "x-man-page"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalOpen {
    Folder(PathBuf),
    Runnable(PathBuf),
    Ssh(tty7_core::core::ssh_profile::QuickConnect),
    ManPage(String),
}

/// Parses the strings supplied by gpui's `Application::on_open_urls`. Finder
/// represents document opens as `file:` URLs on macOS, so paths and custom
/// schemes deliberately share this one entry point.
pub fn parse_open_url(raw: &str) -> Result<ExternalOpen, String> {
    let url = url::Url::parse(raw).map_err(|error| format!("invalid URL: {error}"))?;
    match url.scheme() {
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| "the file URL does not name a local path".to_string())?;
            if path.is_dir() {
                Ok(ExternalOpen::Folder(path))
            } else {
                Ok(ExternalOpen::Runnable(path))
            }
        }
        "ssh" => tty7_core::core::ssh_profile::parse_quick_connect(raw)
            .map(ExternalOpen::Ssh)
            .ok_or_else(|| "the SSH URL has no valid host or port".to_string()),
        "x-man-page" => {
            let page = url
                .host_str()
                .filter(|host| !host.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    let path = url.path().trim_matches('/');
                    (!path.is_empty()).then(|| path.to_owned())
                })
                .ok_or_else(|| "the man-page URL has no page name".to_string())?;
            Ok(ExternalOpen::ManPage(page))
        }
        scheme => Err(format!("unsupported URL scheme: {scheme}")),
    }
}

#[cfg(target_os = "macos")]
pub fn set_as_default_terminal() -> Result<(), String> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    // LaunchServices is a subframework of CoreServices. Linking the parent is
    // portable across both the full Xcode SDK and Command Line Tools SDK; the
    // latter has no standalone `LaunchServices.framework` linker path.
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSSetDefaultRoleHandlerForContentType(
            content_type: core_foundation::string::CFStringRef,
            role: u32,
            handler: core_foundation::string::CFStringRef,
        ) -> i32;
        fn LSSetDefaultHandlerForURLScheme(
            scheme: core_foundation::string::CFStringRef,
            handler: core_foundation::string::CFStringRef,
        ) -> i32;
    }

    // This is the conventional macOS definition of "default terminal":
    // iTerm2 makes the same `public.unix-executable` / `Shell` association. A
    // folder is a Viewer/Editor item rather than something a terminal executes,
    // so asking LaunchServices to assign its Shell role is invalid (-50).
    const ROLE_SHELL: u32 = 0x0000_0008;
    let handler = CFString::new(BUNDLE_ID);
    let executable = CFString::new("public.unix-executable");
    let status = unsafe {
        LSSetDefaultRoleHandlerForContentType(
            executable.as_concrete_TypeRef(),
            ROLE_SHELL,
            handler.as_concrete_TypeRef(),
        )
    };
    if status != 0 {
        return Err(format!(
            "could not set the Unix executable handler (LaunchServices status {status})"
        ));
    }
    for scheme in URL_SCHEMES {
        let scheme = CFString::new(scheme);
        let status = unsafe {
            LSSetDefaultHandlerForURLScheme(
                scheme.as_concrete_TypeRef(),
                handler.as_concrete_TypeRef(),
            )
        };
        if status != 0 {
            return Err(format!(
                "could not set the {scheme} URL handler (LaunchServices status {status})"
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn set_as_default_terminal() -> Result<(), String> {
    Err("setting a default terminal is only available on macOS".to_string())
}

#[cfg(test)]
mod tests {
    use super::{ExternalOpen, parse_open_url};

    #[test]
    fn parses_finder_file_urls_and_percent_decodes_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a folder");
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            parse_open_url(&url::Url::from_file_path(&path).unwrap().to_string()).unwrap(),
            ExternalOpen::Folder(path)
        );
    }

    #[test]
    fn parses_ssh_authority_and_port() {
        let ExternalOpen::Ssh(ssh) = parse_open_url("ssh://me@example.test:2200").unwrap() else {
            panic!("expected SSH request");
        };
        assert_eq!(ssh.user.as_deref(), Some("me"));
        assert_eq!(ssh.host, "example.test");
        assert_eq!(ssh.port, Some(2200));
    }

    #[test]
    fn parses_man_page_host_or_path() {
        assert_eq!(
            parse_open_url("x-man-page://printf").unwrap(),
            ExternalOpen::ManPage("printf".into())
        );
        assert_eq!(
            parse_open_url("x-man-page:/ls").unwrap(),
            ExternalOpen::ManPage("ls".into())
        );
    }
}
