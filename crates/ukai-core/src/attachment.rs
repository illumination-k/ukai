//! Files the user attaches to a prompt, turned into what the agent accepts.

use std::path::Path;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use ukai_acp::{Attachment, PromptCaps};

/// Largest image sent inline.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Largest text file embedded in a prompt; bigger ones are sent as links.
pub const MAX_EMBEDDED_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    #[error("{name}: this agent does not accept images")]
    ImagesUnsupported { name: String },
    #[error("{name}: images over {} MiB are not sent", MAX_IMAGE_BYTES / 1024 / 1024)]
    TooLarge { name: String },
}

/// What to send for the file at `path` with contents `bytes`: an image
/// inline, a text file embedded when the agent takes embedded context and it
/// is small enough, anything else as a link to the file.
pub fn attachment(
    path: &Path,
    bytes: Vec<u8>,
    caps: PromptCaps,
) -> Result<Attachment, AttachError> {
    let name = file_name(path);
    if let Some(mime_type) = image_type(path) {
        if !caps.image {
            return Err(AttachError::ImagesUnsupported { name });
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(AttachError::TooLarge { name });
        }
        return Ok(Attachment::Image {
            mime_type: mime_type.to_owned(),
            data: bytes,
        });
    }
    let uri = file_uri(path);
    if caps.embedded_context
        && bytes.len() <= MAX_EMBEDDED_BYTES
        && let Ok(text) = String::from_utf8(bytes)
    {
        return Ok(Attachment::Text { uri, text });
    }
    Ok(Attachment::Link { name, uri })
}

/// The last component of `path`, for showing an attachment.
pub fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn image_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// Bytes escaped in a `file:` URI path.
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// A `file://` URI for an absolute `path`.
pub fn file_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let slash = if path.starts_with('/') { "" } else { "/" };
    format!("file://{slash}{}", utf8_percent_encode(&path, PATH))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rstest::rstest;

    use super::*;

    const ALL: PromptCaps = PromptCaps {
        image: true,
        embedded_context: true,
        steering: false,
    };
    const NONE: PromptCaps = PromptCaps {
        image: false,
        embedded_context: false,
        steering: false,
    };

    #[rstest]
    #[case::image(
        "/p/shot.PNG", b"png".to_vec(), ALL,
        Ok(Attachment::Image { mime_type: "image/png".into(), data: b"png".to_vec() }),
    )]
    #[case::image_unsupported(
        "/p/a.jpg", vec![], NONE,
        Err(AttachError::ImagesUnsupported { name: "a.jpg".into() }),
    )]
    #[case::image_too_large(
        "/p/a.webp", vec![0; MAX_IMAGE_BYTES + 1], ALL,
        Err(AttachError::TooLarge { name: "a.webp".into() }),
    )]
    #[case::text_embedded(
        "/p/notes.md", b"# hi".to_vec(), ALL,
        Ok(Attachment::Text { uri: "file:///p/notes.md".into(), text: "# hi".into() }),
    )]
    #[case::text_without_embedding(
        "/p/notes.md", b"# hi".to_vec(), NONE,
        Ok(Attachment::Link { name: "notes.md".into(), uri: "file:///p/notes.md".into() }),
    )]
    #[case::text_too_large(
        "/p/big.log", vec![b'a'; MAX_EMBEDDED_BYTES + 1], ALL,
        Ok(Attachment::Link { name: "big.log".into(), uri: "file:///p/big.log".into() }),
    )]
    #[case::binary(
        "/p/a.bin", vec![0xff, 0xfe], ALL,
        Ok(Attachment::Link { name: "a.bin".into(), uri: "file:///p/a.bin".into() }),
    )]
    fn chooses_how_to_send(
        #[case] path: &str,
        #[case] bytes: Vec<u8>,
        #[case] caps: PromptCaps,
        #[case] expected: Result<Attachment, AttachError>,
    ) {
        assert_eq!(attachment(&PathBuf::from(path), bytes, caps), expected);
    }

    #[rstest]
    #[case::plain("/p/a.txt", "file:///p/a.txt")]
    #[case::escaped("/p/my file#1.txt", "file:///p/my%20file%231.txt")]
    #[case::unicode("/p/鵜.txt", "file:///p/%E9%B5%9C.txt")]
    #[case::windows(r"C:\p\a.txt", "file:///C:/p/a.txt")]
    fn file_uris(#[case] path: &str, #[case] expected: &str) {
        assert_eq!(file_uri(Path::new(path)), expected);
    }
}
