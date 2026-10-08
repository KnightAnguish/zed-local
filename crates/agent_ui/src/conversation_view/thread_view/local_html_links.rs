use std::path::{Path, PathBuf};

fn is_html(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            value.eq_ignore_ascii_case("html") || value.eq_ignore_ascii_case("htm")
        })
}

pub(super) fn browser_url(
    link: &str,
    mut resolve: impl FnMut(&str) -> Option<PathBuf>,
) -> Option<url::Url> {
    if link.starts_with("file:") {
        let mut url = url::Url::parse(link).ok()?;
        if url.host_str().is_some() || !is_html(&url.to_file_path().ok()?) {
            return None;
        }
        if url.fragment().is_some_and(is_source_line) {
            url.set_fragment(None);
        }
        return Some(url);
    }
    // Let the existing dispatcher handle web URLs and non-file mention URIs.
    if link.contains("://") {
        return None;
    }
    let (raw_path, fragment) = link
        .split_once('#')
        .map_or((link, None), |(path, fragment)| (path, Some(fragment)));
    let decoded = percent_encoding::percent_decode_str(raw_path)
        .decode_utf8()
        .ok()?;
    if !is_html(Path::new(&*decoded)) {
        return None;
    }
    let resolve_path = |path: &str, resolve: &mut dyn FnMut(&str) -> Option<PathBuf>| {
        let path_value = Path::new(path);
        if path_value.is_absolute() {
            Some(path_value.to_path_buf())
        } else {
            resolve(path)
        }
    };
    let path =
        resolve_path(&decoded, &mut resolve).or_else(|| resolve_path(raw_path, &mut resolve))?;
    let path = if path.is_absolute() && !path.exists() && Path::new(raw_path).is_file() {
        PathBuf::from(raw_path)
    } else {
        path
    };
    let mut url = url::Url::from_file_path(path).ok()?;
    if url.host_str().is_some() {
        return None;
    }
    url.set_fragment(fragment.filter(|fragment| !is_source_line(fragment)));
    Some(url)
}

fn is_source_line(fragment: &str) -> bool {
    fragment
        .strip_prefix('L')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_links_and_non_html() {
        assert!(browser_url("https://example.com/index.html", |_| None).is_none());
        assert!(browser_url("file:///C:/image.png", |_| None).is_none());
        assert!(browser_url("file://server/share/index.html", |_| None).is_none());
        assert!(
            browser_url("file:///C:/a%20b.HTML#L20", |_| None)
                .expect("file URL")
                .fragment()
                .is_none()
        );
        assert_eq!(
            browser_url("file:///C:/a.htm#compare", |_| None)
                .expect("file URL")
                .fragment(),
            Some("compare")
        );
    }
    #[cfg(windows)]
    #[test]
    fn windows_paths_and_relative_resolution() {
        for path in [
            "C:/a b/слайдер.html",
            r"C:\a b\слайдер.html",
            "C:/a%20b/слайдер.html",
        ] {
            let url = browser_url(path, |_| None).expect("Windows path");
            assert_eq!(
                url.to_file_path().expect("path"),
                PathBuf::from(r"C:\a b\слайдер.html")
            );
        }
        let url = browser_url("tmp/slider.html#compare", |path| {
            (path == "tmp/slider.html").then(|| PathBuf::from("C:/project/tmp/slider.html"))
        })
        .expect("project path");
        assert_eq!(url.fragment(), Some("compare"));
        let url = browser_url("tmp/a%20b.html", |path| {
            (path == "tmp/a b.html").then(|| PathBuf::from("C:/project/tmp/a b.html"))
        })
        .expect("encoded project path");
        assert!(url.as_str().contains("a%20b.html"));
    }
}
