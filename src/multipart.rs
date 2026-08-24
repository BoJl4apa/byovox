//! multipart/form-data body builder — the ~30 lines that let us stay on `ureq`
//! without an async runtime.

pub struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self::with_boundary(&format!("byovox{nanos:x}{:x}", std::process::id()))
    }

    pub fn with_boundary(boundary: &str) -> Self {
        Self {
            boundary: boundary.to_string(),
            body: Vec::new(),
        }
    }

    pub fn text(&mut self, name: &str, value: &str) {
        self.body
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }

    pub fn file(&mut self, name: &str, filename: &str, content_type: &str, bytes: &[u8]) {
        self.body
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
    }

    pub fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

impl Default for Multipart {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_byte_exact() {
        let mut m = Multipart::with_boundary("XYZ");
        m.text("model", "whisper-1");
        m.file("file", "clip.wav", "audio/wav", b"RIFF");
        let (ct, body) = m.finish();
        assert_eq!(ct, "multipart/form-data; boundary=XYZ");
        let expected = "--XYZ\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n\
                        --XYZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n\
                        --XYZ--\r\n";
        assert_eq!(body, expected.as_bytes());
    }
}
