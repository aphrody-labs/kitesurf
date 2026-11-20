// Copyright (c) 2026 Cloudflare, Inc.
// Licensed under the Apache 2.0 license found in the LICENSE file or at:
// https://opensource.org/licenses/Apache-2.0

use crate::render_html_to_image;

fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .expect("png info");
    let mut pixels = vec![0u8; reader.output_buffer_size().expect("pixel buffer size")];
    let info = reader.next_frame(&mut pixels).expect("png frame");
    (info.width, info.height, pixels)
}

#[test]
fn renders_page_pixels_to_png() {
    let html = r#"<!doctype html><html><head><style>
  html, body { margin: 0; background: rgb(0, 0, 255); }
  div { width: 20px; height: 10px; background: rgb(255, 0, 0); }
</style></head><body><div></div></body></html>"#;
    let png =
        render_html_to_image(html, "https://test.example/", 40, 30, "png", 90).expect("render");
    let (w, h, px) = decode(&png);
    assert_eq!((w, h), (40, 30));
    let at = |x: usize, y: usize| &px[(y * 40 + x) * 4..][..4];
    assert_eq!(at(5, 5), [255, 0, 0, 255], "inside the red box");
    assert_eq!(
        at(30, 5),
        [0, 0, 255, 255],
        "page background right of the box"
    );
    assert_eq!(at(5, 20), [0, 0, 255, 255], "page background below the box");
}

#[test]
fn rejects_an_empty_viewport() {
    assert!(render_html_to_image("<p>x</p>", "", 0, 10, "png", 90).is_err());
}
