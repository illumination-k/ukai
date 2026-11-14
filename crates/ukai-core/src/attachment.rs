//! Files the user attaches to a prompt, turned into what the agent accepts.

use std::path::Path;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use ukai_acp::checkout::MAX_UPLOAD_BYTES;
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
    #[error("{name}: files over {} MiB are not uploaded", MAX_UPLOAD_BYTES / 1024 / 1024)]
    UploadTooLarge { name: String },
}

/// How a file gets to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attach {
    /// In the prompt, as it is.
    Send(Attachment),
    /// The agent cannot read this machine's files: the file is uploaded to
    /// where it works first, and the prompt links that copy ([`uploaded`]).
    Upload { name: String, bytes: Vec<u8> },
}

/// What to send for the file at `path` with contents `bytes`: an image
/// inline, a text file embedded when the agent takes embedded context and it
/// is small enough, anything else as a link to the file. An agent that is
/// `remote` could not follow that link, so the file is uploaded instead.
pub fn attachment(
    path: &Path,
    bytes: Vec<u8>,
    caps: PromptCaps,
    remote: bool,
) -> Result<Attach, AttachError> {
    let name = file_name(path);
    if let Some(mime_type) = image_type(path) {
        if !caps.image {
            return Err(AttachError::ImagesUnsupported { name });
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(AttachError::TooLarge { name });
        }
        return Ok(Attach::Send(Attachment::Image {
            mime_type: mime_type.to_owned(),
            data: bytes,
        }));
    }
    let uri = file_uri(path);
    let bytes = if caps.embedded_context && bytes.len() <= MAX_EMBEDDED_BYTES {
        match String::from_utf8(bytes) {
            Ok(text) => return Ok(Attach::Send(Attachment::Text { uri, text })),
            Err(binary) => binary.into_bytes(),
        }
    } else {
        bytes
    };
    if !remote {
        return Ok(Attach::Send(Attachment::Link { name, uri }));
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(AttachError::UploadTooLarge { name });
    }
    Ok(Attach::Upload { name, bytes })
}

/// The link to send for a file uploaded as `name` to `path`, where the
/// agent works.
pub fn uploaded(name: String, path: &str) -> Attachment {
    Attachment::Link {
        name,
        uri: file_uri(Path::new(path)),
    }
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

    fn link(name: &str, uri: &str) -> Attach {
        Attach::Send(Attachment::Link {
            name: name.into(),
            uri: uri.into(),
        })
    }

    #[rstest]
    #[case::image(
        "/p/shot.PNG", b"png".to_vec(), ALL, false,
        Ok(Attach::Send(Attachment::Image { mime_type: "image/png".into(), data: b"png".to_vec() })),
    )]
    #[case::image_unsupported(
        "/p/a.jpg", vec![], NONE, false,
        Err(AttachError::ImagesUnsupported { name: "a.jpg".into() }),
    )]
    #[case::image_too_large(
        "/p/a.webp", vec![0; MAX_IMAGE_BYTES + 1], ALL, false,
        Err(AttachError::TooLarge { name: "a.webp".into() }),
    )]
    #[case::text_embedded(
        "/p/notes.md", b"# hi".to_vec(), ALL, false,
        Ok(Attach::Send(Attachment::Text { uri: "file:///p/notes.md".into(), text: "# hi".into() })),
    )]
    #[case::text_embedded_remote(
        "/p/notes.md", b"# hi".to_vec(), ALL, true,
        Ok(Attach::Send(Attachment::Text { uri: "file:///p/notes.md".into(), text: "# hi".into() })),
    )]
    #[case::text_without_embedding(
        "/p/notes.md", b"# hi".to_vec(), NONE, false,
        Ok(link("notes.md", "file:///p/notes.md")),
    )]
    #[case::text_without_embedding_remote(
        "/p/notes.md", b"# hi".to_vec(), NONE, true,
        Ok(Attach::Upload { name: "notes.md".into(), bytes: b"# hi".to_vec() }),
    )]
    #[case::text_too_large(
        "/p/big.log", vec![b'a'; MAX_EMBEDDED_BYTES + 1], ALL, false,
        Ok(link("big.log", "file:///p/big.log")),
    )]
    #[case::text_too_large_remote(
        "/p/big.log", vec![b'a'; MAX_EMBEDDED_BYTES + 1], ALL, true,
        Ok(Attach::Upload { name: "big.log".into(), bytes: vec![b'a'; MAX_EMBEDDED_BYTES + 1] }),
    )]
    #[case::binary(
        "/p/a.bin", vec![0xff, 0xfe], ALL, false,
        Ok(link("a.bin", "file:///p/a.bin")),
    )]
    #[case::binary_remote(
        "/p/a.bin", vec![0xff, 0xfe], ALL, true,
        Ok(Attach::Upload { name: "a.bin".into(), bytes: vec![0xff, 0xfe] }),
    )]
    #[case::upload_too_large(
        "/p/a.bin", vec![0; MAX_UPLOAD_BYTES + 1], ALL, true,
        Err(AttachError::UploadTooLarge { name: "a.bin".into() }),
    )]
    fn chooses_how_to_send(
        #[case] path: &str,
        #[case] bytes: Vec<u8>,
        #[case] caps: PromptCaps,
        #[case] remote: bool,
        #[case] expected: Result<Attach, AttachError>,
    ) {
        assert_eq!(
            attachment(&PathBuf::from(path), bytes, caps, remote),
            expected
        );
    }

    #[test]
    fn an_uploaded_file_is_linked_where_the_agent_reads_it() {
        assert_eq!(
            uploaded("my paper.pdf".into(), "/tmp/ukai/0/my paper.pdf"),
            Attachment::Link {
                name: "my paper.pdf".into(),
                uri: "file:///tmp/ukai/0/my%20paper.pdf".into(),
            }
        );
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
