use gpui::{
    App, AppContext, AvailableSpace, Context, IntoElement, RenderImage, RenderOnce, StyledImage,
    Task, Window, canvas, img,
};
use markdown::{
    Markdown,
    parser::{MarkdownEvent, MarkdownTag, MarkdownTagEnd},
};
use std::{
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{Tooltip, prelude::*};

const MAX_PREVIEWS: usize = 6;
static DECODE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn is_image_path(value: &str) -> bool {
    let extension = value.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
    )
}

fn local_path(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches(|c| matches!(c, '<' | '>' | '`' | '"' | '\''));
    let path = if value.starts_with("file:") {
        url::Url::parse(value)
            .ok()?
            .to_file_path()
            .ok()?
            .to_string_lossy()
            .into_owned()
    } else {
        if value.contains("://") || value.starts_with("//") || value.starts_with("\\\\") {
            return None;
        }
        let mut decoded = Vec::with_capacity(value.len());
        let mut bytes = value.as_bytes().iter().copied().peekable();
        while let Some(byte) = bytes.next() {
            if byte == b'%' {
                let high = char::from(bytes.next()?).to_digit(16)?;
                let low = char::from(bytes.next()?).to_digit(16)?;
                decoded.push((high * 16 + low) as u8);
            } else {
                decoded.push(byte);
            }
        }
        String::from_utf8(decoded).ok()?
    };
    if path.starts_with("//") || path.starts_with("\\\\") {
        return None;
    }
    is_image_path(&path).then_some(path)
}

fn bare_paths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (start, character) in text.char_indices() {
        let tail = &text[start..];
        let boundary = start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || matches!(c, '(' | '<' | '"' | '\'' | '`'));
        let drive = character.is_ascii_alphabetic()
            && (tail.as_bytes().get(1) == Some(&b':'))
            && matches!(tail.as_bytes().get(2), Some(b'/' | b'\\'));
        if !boundary
            || !(drive
                || tail.starts_with('/')
                || tail.starts_with("./")
                || tail.starts_with("../"))
        {
            continue;
        }
        for (offset, c) in tail.char_indices() {
            if c == '\n' || c == '\r' || c == '`' || c == '<' || c == '>' || c == '"' {
                break;
            }
            if c != '.' {
                continue;
            }
            for extension in ["png", "jpeg", "jpg", "webp", "gif", "bmp"] {
                let end = offset + 1 + extension.len();
                if tail
                    .get(offset + 1..end)
                    .is_some_and(|part| part.eq_ignore_ascii_case(extension))
                    && tail.get(end..).is_some_and(|rest| {
                        let rest = rest.trim_start_matches('.');
                        rest.is_empty()
                            || rest.starts_with(|c: char| {
                                c.is_whitespace()
                                    || matches!(c, ')' | ']' | ',' | ';' | '!' | '?' | '"' | '\'')
                            })
                    })
                {
                    if let Some(path) = local_path(&tail[..end]) {
                        found.push(path);
                    }
                    break;
                }
            }
            if !found.is_empty() && found.last().is_some_and(|path| tail.starts_with(path)) {
                break;
            }
        }
        if found.len() >= MAX_PREVIEWS {
            break;
        }
    }
    found
}

pub(crate) fn image_links(markdown: &Markdown) -> Vec<String> {
    let parsed = markdown.parsed_markdown();
    let mut found = Vec::new();
    let mut inline_images = Vec::new();
    let mut nested_link = false;
    let mut code_block = false;
    for (range, event) in parsed.events().iter() {
        match event {
            MarkdownEvent::Start(MarkdownTag::CodeBlock { .. }) => code_block = true,
            MarkdownEvent::End(MarkdownTagEnd::CodeBlock) => code_block = false,
            MarkdownEvent::Start(MarkdownTag::Image { dest_url, .. }) => {
                nested_link = true;
                if let Some(path) = local_path(dest_url) {
                    inline_images.push(path);
                }
            }
            MarkdownEvent::Start(MarkdownTag::Link { dest_url, .. }) => {
                nested_link = true;
                if let Some(path) = local_path(dest_url) {
                    found.push(path);
                }
            }
            MarkdownEvent::End(MarkdownTagEnd::Link | MarkdownTagEnd::Image) => nested_link = false,
            MarkdownEvent::Code | MarkdownEvent::Text if !nested_link && !code_block => {
                if let Some(text) = parsed.source().get(range.clone()) {
                    let text = text.trim_matches('`');
                    found.extend(bare_paths(text));
                }
            }
            _ => {}
        }
    }
    let mut unique = Vec::new();
    for path in found {
        if !inline_images.contains(&path) && !unique.contains(&path) {
            unique.push(path);
            if unique.len() == MAX_PREVIEWS {
                break;
            }
        }
    }
    unique
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct ThumbnailSource {
    path: String,
    roots: Vec<PathBuf>,
}

#[derive(Default)]
struct UnavailablePreviews(std::collections::VecDeque<ThumbnailSource>);
impl gpui::Global for UnavailablePreviews {}

// Keep only small layout metadata between visits, never decoded pixels.
#[derive(Default)]
struct PreviewWidths(std::collections::VecDeque<(ThumbnailSource, f32)>);
impl gpui::Global for PreviewWidths {}

fn preview_width(source: &ThumbnailSource, cx: &mut App) -> gpui::Pixels {
    px(cx
        .default_global::<PreviewWidths>()
        .0
        .iter()
        .find(|(entry, _)| entry == source)
        .map_or(220., |(_, width)| *width))
}

pub(crate) fn image_row() -> gpui::Div {
    h_flex().w_full().flex_wrap().items_start().gap_2()
}

fn preview_height(source: &ThumbnailSource, cx: &mut App) -> gpui::Pixels {
    if cx
        .default_global::<UnavailablePreviews>()
        .0
        .contains(source)
    {
        px(24.)
    } else {
        px(220.)
    }
}

struct VisibleThumbnail {
    result: Option<Result<Arc<RenderImage>, Arc<str>>>,
    _load: Task<()>,
}

impl VisibleThumbnail {
    fn for_bounds(
        source: ThumbnailSource,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::Entity<Self>> {
        if bounds.intersect(&window.content_mask().bounds).is_empty() {
            return None;
        }
        let key = SharedString::from(format!("visible-preview-{}", source.path));
        Some(window.use_keyed_state(key, cx, |_, cx| Self::new(source, cx)))
    }

    fn new(source: ThumbnailSource, cx: &mut Context<Self>) -> Self {
        cx.on_release(|state, cx| {
            if let Some(Ok(image)) = state.result.take() {
                cx.drop_image(image, None);
            }
        })
        .detach();
        let decode_source = source.clone();
        let decode = cx.background_spawn(async move {
            load_thumbnail(&decode_source).map_err(|error| Arc::<str>::from(error.to_string()))
        });
        let load = cx.spawn(async move |this, cx| {
            let result = decode.await;
            if let Err(error) = this.update(cx, |state, cx| {
                if let Ok(image) = &result {
                    let size = image.size(0);
                    let widths = cx.default_global::<PreviewWidths>();
                    widths.0.retain(|(entry, _)| entry != &source);
                    widths.0.push_back((
                        source.clone(),
                        (220. * size.width.0 as f32 / size.height.0.max(1) as f32)
                            .clamp(160., 512.),
                    ));
                    if widths.0.len() > 256 {
                        widths.0.pop_front();
                    }
                }
                let unavailable = cx.default_global::<UnavailablePreviews>();
                unavailable.0.retain(|entry| entry != &source);
                if result.is_err() {
                    unavailable.0.push_back(source);
                    if unavailable.0.len() > 256 {
                        unavailable.0.pop_front();
                    }
                }
                state.result = Some(result);
                cx.notify();
            }) {
                log::debug!("Preview released before decode completed: {error}");
            }
        });
        Self {
            result: None,
            _load: load,
        }
    }
}

fn load_thumbnail(source: &ThumbnailSource) -> anyhow::Result<Arc<RenderImage>> {
    // Decode one original at a time: several large links must not multiply peak RAM.
    let _guard = DECODE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Image decoder unavailable"))?;
    let requested = Path::new(&source.path);
    let path = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        anyhow::ensure!(
            !source.path.contains(':'),
            "This image is on another device"
        );
        source
            .roots
            .iter()
            .map(|root| root.join(requested))
            .find(|path| path.is_file())
            .ok_or_else(|| anyhow::anyhow!("Image is not available on this device"))?
    };
    let mut data = decode_thumbnail(&path)?;
    // GPUI textures use BGRA, unlike the decoder's RGBA output.
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(data)])))
}

fn decode_thumbnail(path: &Path) -> anyhow::Result<image::RgbaImage> {
    let file = std::fs::File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.len() <= 32 * 1024 * 1024,
        "Image is too large for a preview"
    );
    let mut reader = image::ImageReader::new(BufReader::new(file)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?.thumbnail(512, 320).into_rgba8())
}

#[derive(IntoElement)]
pub(crate) struct ChatImagePreview {
    path: String,
    roots: Vec<PathBuf>,
    attachment: bool,
}

impl ChatImagePreview {
    pub(crate) fn new(path: String, roots: Vec<PathBuf>) -> Self {
        Self {
            path,
            roots,
            attachment: false,
        }
    }

    pub(crate) fn attachment(mut self) -> Self {
        self.attachment = true;
        self
    }
}

impl RenderOnce for ChatImagePreview {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let source = ThumbnailSource {
            path: self.path.clone(),
            roots: self.roots,
        };
        let filename = self
            .path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&self.path)
            .to_string();
        let key = SharedString::from(format!("visible-preview-{}", self.path));
        let height = preview_height(&source, cx);
        let width = preview_width(&source, cx);
        let action_path = self.path.clone();
        v_flex()
            .debug_selector(move || format!("image-preview-{}", self.path))
            .w(width)
            .max_w_full()
            .flex_none()
            .gap_1()
            .my_2()
            .child(
                canvas(
                    move |bounds, window, cx| {
                        // Do not keep decoded pixels or GPU textures for clipped images, even
                        // when a long message containing them is still part of the virtual list.
                        let state = VisibleThumbnail::for_bounds(source, bounds, window, cx)?;
                        #[cfg(test)]
                        let loaded = matches!(state.read(cx).result, Some(Ok(_)));
                        let mut content = match state.read(cx).result.clone() {
                            Some(Ok(image)) => img(image)
                                // The image's intrinsic ratio otherwise overrides the
                                // slot's height during standalone root layout.
                                .w(bounds.size.width)
                                .h(bounds.size.height)
                                .aspect_ratio(bounds.size.width / bounds.size.height)
                                .object_fit(gpui::ObjectFit::Contain)
                                .into_any_element(),
                            Some(Err(error)) => div()
                                .id(key)
                                .child(
                                    Label::new("Preview unavailable on this device")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text(error.to_string()))
                                .into_any_element(),
                            None => div()
                                .child(
                                    Label::new("Loading image…")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .into_any_element(),
                        };
                        let _content_size = content.layout_as_root(
                            bounds.size.map(AvailableSpace::Definite),
                            window,
                            cx,
                        );
                        #[cfg(test)]
                        if loaded {
                            assert_eq!(
                                _content_size, bounds.size,
                                "Preview must stay inside its slot"
                            );
                        }
                        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                            content.prepaint_at(bounds.origin, window, cx);
                        });
                        Some(content)
                    },
                    |bounds, content, window, cx| {
                        if let Some(mut content) = content {
                            window.with_content_mask(
                                Some(gpui::ContentMask { bounds }),
                                |window| {
                                    content.paint(window, cx);
                                },
                            );
                        }
                    },
                )
                .w_full()
                .flex_none()
                .h(height),
            )
            .when(!self.attachment, |element| {
                element.child(
                    Label::new(filename)
                        .truncate()
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(self.attachment, |element| {
                element.child(attachment_actions(action_path))
            })
    }
}

fn attachment_actions(path: String) -> impl IntoElement {
    let open_path = path.clone();
    let copy_path = path.clone();
    h_flex()
        .id(SharedString::from(format!("attachment-actions-{path}")))
        .flex_wrap()
        .gap_1()
        .child(
            Button::new("open-attachment", "Show file")
                .label_size(LabelSize::XSmall)
                .on_click(move |_, _, cx| {
                    cx.reveal_path(Path::new(&open_path));
                }),
        )
        .child(
            Button::new("copy-attachment", "Copy image")
                .label_size(LabelSize::XSmall)
                .on_click(move |_, window, cx| {
                    let path = copy_path.clone();
                    let window = window.window_handle();
                    let read = cx.background_spawn(async move {
                        let file = std::fs::File::open(path)?;
                        anyhow::ensure!(
                            file.metadata()?.len() <= 32 * 1024 * 1024,
                            "Image is too large"
                        );
                        let mut bytes = Vec::new();
                        std::io::Read::read_to_end(
                            &mut std::io::Read::take(file, 32 * 1024 * 1024 + 1),
                            &mut bytes,
                        )?;
                        anyhow::ensure!(bytes.len() <= 32 * 1024 * 1024, "Image is too large");
                        let format = gpui::ImageFormat::from_mime_type(
                            image::guess_format(&bytes)?.to_mime_type(),
                        )
                        .ok_or_else(|| anyhow::anyhow!("Unsupported image format"))?;
                        anyhow::Ok(gpui::Image::from_bytes(format, bytes))
                    });
                    cx.spawn(async move |cx| {
                        match read.await {
                            Ok(image) => cx.update(|cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image))
                            }),
                            Err(error) => window.update(cx, |_, window, cx| {
                                drop(window.prompt(
                                    gpui::PromptLevel::Warning,
                                    "Could not copy image",
                                    Some(&error.to_string()),
                                    &["OK"],
                                    cx,
                                ));
                            })?,
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }),
        )
        .child(
            Button::new("save-attachment", "Save as")
                .label_size(LabelSize::XSmall)
                .on_click(move |_, window, cx| {
                    let source = PathBuf::from(&path);
                    let choose = cx.prompt_for_new_path(
                        source.parent().unwrap_or(Path::new(".")),
                        source.file_name().and_then(|name| name.to_str()),
                    );
                    let window = window.window_handle();
                    cx.spawn(async move |cx| {
                        let result: anyhow::Result<()> = async {
                            if let Some(destination) = choose.await?? {
                                cx.background_spawn(async move {
                                    anyhow::ensure!(
                                        source != destination,
                                        "Choose a different destination"
                                    );
                                    let mut input = std::fs::File::open(source)?;
                                    let mut output = std::fs::OpenOptions::new()
                                        .write(true)
                                        .create_new(true)
                                        .open(destination)?;
                                    std::io::copy(&mut input, &mut output)?;
                                    output.sync_all()?;
                                    anyhow::Ok(())
                                })
                                .await?;
                            }
                            Ok(())
                        }
                        .await;
                        if let Err(error) = result {
                            window.update(cx, |_, window, cx| {
                                drop(window.prompt(
                                    gpui::PromptLevel::Warning,
                                    "Could not save image",
                                    Some(&error.to_string()),
                                    &["OK"],
                                    cx,
                                ));
                            })?;
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }),
        )
}
#[cfg(test)]
mod tests {
    use super::*;

    struct VisibilityProbe {
        offset: f32,
        path: String,
        observed: std::rc::Rc<std::cell::RefCell<Vec<gpui::WeakEntity<VisibleThumbnail>>>>,
    }

    impl gpui::Render for VisibilityProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let offset = self.offset;
            let path = self.path.clone();
            let observed = self.observed.clone();
            canvas(
                move |mut bounds, window, cx| {
                    bounds.origin.y += px(offset);
                    if let Some(state) = VisibleThumbnail::for_bounds(
                        ThumbnailSource {
                            path,
                            roots: vec![],
                        },
                        bounds,
                        window,
                        cx,
                    ) {
                        observed.borrow_mut().push(state.downgrade());
                    }
                },
                |_, _, _, _| {},
            )
            .w(px(200.))
            .h(px(220.))
        }
    }

    #[gpui::test]
    fn releases_offscreen_and_reloads_on_return(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().expect("temporary image directory");
        let path = directory.path().join("preview.png");
        image::RgbaImage::from_pixel(1024, 640, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .expect("initial image");
        let observed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let window = cx.add_window(|_, _| VisibilityProbe {
            offset: 100_000.,
            path: path.to_string_lossy().into_owned(),
            observed: observed.clone(),
        });
        cx.run_until_parked();
        assert!(
            observed.borrow().is_empty(),
            "Offscreen images must not load"
        );

        window
            .update(cx, |view, _, cx| {
                view.offset = 0.;
                cx.notify();
            })
            .expect("show");
        cx.run_until_parked();
        let first = observed.borrow().last().cloned().expect("visible preview");
        assert!(first.upgrade().is_some());
        let pixels = first
            .read_with(cx, |state, _| {
                let image = state
                    .result
                    .as_ref()
                    .expect("loaded")
                    .as_ref()
                    .expect("decoded");
                assert_eq!(image.size(0), gpui::size(512u32.into(), 320u32.into()));
                assert_eq!(
                    image.as_bytes(0).expect("pixels").get(..4),
                    Some(&[0, 0, 255, 255][..])
                );
                Arc::downgrade(image)
            })
            .expect("preview state");

        window
            .update(cx, |view, _, cx| {
                view.offset = 100_000.;
                cx.notify();
            })
            .expect("hide");
        cx.run_until_parked();
        // Entity releases are flushed in the next application effect cycle.
        cx.update(|_| {});
        assert!(
            first.upgrade().is_none(),
            "Clipped preview must release its state"
        );
        assert!(
            pixels.upgrade().is_none(),
            "Decoded pixels must be released"
        );
        image::RgbaImage::from_pixel(32, 16, image::Rgba([0, 255, 0, 255]))
            .save(&path)
            .expect("overwrite image at the same path");

        window
            .update(cx, |view, _, cx| {
                view.offset = 0.;
                cx.notify();
            })
            .expect("return");
        cx.run_until_parked();
        let second = observed.borrow().last().cloned().expect("new preview");
        assert!(second.upgrade().is_some());
        second
            .read_with(cx, |state, _| {
                let image = state
                    .result
                    .as_ref()
                    .expect("reloaded")
                    .as_ref()
                    .expect("decoded");
                assert_eq!(image.size(0), gpui::size(512u32.into(), 256u32.into()));
                assert_eq!(
                    image.as_bytes(0).expect("pixels").get(..4),
                    Some(&[0, 255, 0, 255][..])
                );
            })
            .expect("new preview state");
        assert_ne!(
            first.entity_id(),
            second.entity_id(),
            "Returning must read the file anew"
        );
    }

    struct PreviewView(String);

    struct GalleryView {
        paths: Vec<String>,
        width: f32,
    }

    impl gpui::Render for GalleryView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).child(
                image_row().children(
                    self.paths
                        .iter()
                        .map(|path| ChatImagePreview::new(path.clone(), vec![])),
                ),
            )
        }
    }

    #[gpui::test]
    fn gallery_wraps_in_order_and_fits_narrow_parent(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);
        let directory = tempfile::tempdir().expect("temporary images");
        let paths: Vec<String> = [(100, 100), (100, 100), (160, 90)]
            .into_iter()
            .enumerate()
            .map(|(ix, (width, height))| {
                let path = directory.path().join(format!("{ix}.png"));
                image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
                    .save(&path)
                    .expect("image");
                path.to_string_lossy().into_owned()
            })
            .collect();
        let (view, cx) = cx.add_window_view(|_, _| GalleryView {
            paths: paths.clone(),
            width: 500.,
        });
        cx.run_until_parked();
        let selectors: Vec<&'static str> = paths
            .iter()
            .map(|path| &*Box::leak(format!("image-preview-{path}").into_boxed_str()))
            .collect();
        let bounds = |cx: &mut gpui::VisualTestContext, ix: usize| {
            cx.debug_bounds(selectors[ix]).expect("preview bounds")
        };
        let first = bounds(cx, 0);
        let second = bounds(cx, 1);
        let third = bounds(cx, 2);
        assert_eq!(first.size.width, px(220.));
        assert_eq!(first.origin.y, second.origin.y);
        assert!(second.origin.x >= first.right());
        assert!(third.origin.y >= first.bottom());
        assert!(third.size.width > first.size.width);
        view.update(cx, |view, cx| {
            view.width = 180.;
            cx.notify();
        });
        cx.run_until_parked();
        for ix in 0..3 {
            let current = bounds(cx, ix);
            assert!(current.size.width <= px(180.));
            if ix > 0 {
                assert!(current.origin.y >= bounds(cx, ix - 1).bottom());
            }
        }
        view.update(cx, |view, cx| {
            view.width = 500.;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(bounds(cx, 0).origin.y, bounds(cx, 1).origin.y);
    }

    impl gpui::Render for PreviewView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            ChatImagePreview::new(self.0.clone(), Vec::new())
        }
    }

    #[gpui::test]
    fn renders_loaded_and_unavailable_preview(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);
        let directory = tempfile::tempdir().expect("temporary image directory");
        let path = directory.path().join("preview.png");
        image::RgbaImage::from_pixel(128, 64, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .expect("image");
        let window = cx.add_window(|_, _| PreviewView(path.to_string_lossy().into_owned()));
        cx.run_until_parked();
        for (width, height) in [(100, 900), (900, 100), (512, 512)] {
            let path = directory.path().join(format!("{width}-{height}.png"));
            image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
                .save(&path)
                .expect("image with different aspect ratio");
            window
                .update(cx, |view, _, cx| {
                    view.0 = path.to_string_lossy().into_owned();
                    cx.notify();
                })
                .expect("change aspect ratio");
            cx.run_until_parked();
        }
        window
            .update(cx, |view, _, cx| {
                view.0 = directory
                    .path()
                    .join("missing.png")
                    .to_string_lossy()
                    .into_owned();
                cx.notify();
            })
            .expect("missing image");
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                preview_height(
                    &ThumbnailSource {
                        path: directory
                            .path()
                            .join("missing.png")
                            .to_string_lossy()
                            .into_owned(),
                        roots: Vec::new(),
                    },
                    cx
                ),
                px(24.)
            );
        });
    }

    #[gpui::test]
    fn markdown_image_links(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);
        for (source, expected) in [
            (
                "C:/Pictures/Лютик_посадка.png",
                vec!["C:/Pictures/Лютик_посадка.png"],
            ),
            (
                "[Image](<C:/My pictures/Лютик.png>)",
                vec!["C:/My pictures/Лютик.png"],
            ),
            ("`C:/Pictures/a.png`", vec!["C:/Pictures/a.png"]),
            (
                "![Inline](C:/Pictures/a.png) [Same](C:/Pictures/a.png)",
                vec![],
            ),
            ("[One](./a.png) [Again](./a.png)", vec!["./a.png"]),
            ("```\nC:/Pictures/a.png\n```", vec![]),
            ("[Remote](https://example.org/a.png)", vec![]),
        ] {
            let markdown = cx.new(|cx| Markdown::new(source.into(), None, None, cx));
            cx.run_until_parked();
            markdown.read_with(cx, |markdown, _| {
                assert_eq!(image_links(markdown), expected, "{source}");
            });
        }
    }

    #[test]
    fn local_paths_and_extensions() {
        assert!(is_image_path("C:/Папка/Лютик.PNG"));
        assert!(!is_image_path("image.png.exe"));
        assert_eq!(local_path("https://example.org/image.png"), None);
        assert_eq!(local_path("//server/share/image.png"), None);
        assert_eq!(local_path("\\\\server\\share\\image.png"), None);
        assert_eq!(
            bare_paths("Смотри C:/Мои картинки/Лютик.png и ./other.jpg."),
            vec!["C:/Мои картинки/Лютик.png", "./other.jpg"]
        );
        assert_eq!(bare_paths("/tmp/test.png.exe"), Vec::<String>::new());
        assert_eq!(
            bare_paths("C:\\Pictures\\one.webp"),
            vec!["C:\\Pictures\\one.webp"]
        );
    }
}
