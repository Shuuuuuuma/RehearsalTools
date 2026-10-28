//! Windows 標準の OCR（Windows.Media.Ocr）．Python 版の winocr と同じものを直接呼ぶ．

use image::RgbImage;
use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

pub struct OcrWord {
    pub y: f32,
}

pub struct OcrLine {
    pub text: String,
    pub words: Vec<OcrWord>,
}

fn engine(lang: &str) -> windows::core::Result<OcrEngine> {
    Language::CreateLanguage(&HSTRING::from(lang))
        .and_then(|l| OcrEngine::TryCreateFromLanguage(&l))
        .or_else(|_| OcrEngine::TryCreateFromUserProfileLanguages())
}

pub fn available(lang: &str) -> Result<(), String> {
    init();
    engine(lang).map(|_| ()).map_err(|e| format!("Windows OCR が使えません: {e}"))
}

fn init() {
    unsafe {
        // すでに初期化済み（S_FALSE / RPC_E_CHANGED_MODE）でも構わない
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

pub fn recognize(img: &RgbImage, lang: &str) -> Result<Vec<OcrLine>, String> {
    init();
    let run = || -> windows::core::Result<Vec<OcrLine>> {
        let eng = engine(lang)?;
        let mut bgra = Vec::with_capacity((img.width() * img.height() * 4) as usize);
        for p in img.pixels() {
            bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        let writer = DataWriter::new()?;
        writer.WriteBytes(&bgra)?;
        let buf = writer.DetachBuffer()?;
        let bmp = SoftwareBitmap::CreateCopyFromBuffer(&buf, BitmapPixelFormat::Bgra8, img.width() as i32, img.height() as i32)?;
        let res = eng.RecognizeAsync(&bmp)?.get()?;
        let lines = res.Lines()?;
        let mut out = Vec::new();
        for i in 0..lines.Size()? {
            let line = lines.GetAt(i)?;
            let words = line.Words()?;
            let mut ws = Vec::new();
            for j in 0..words.Size()? {
                ws.push(OcrWord { y: words.GetAt(j)?.BoundingRect()?.Y });
            }
            out.push(OcrLine { text: line.Text()?.to_string(), words: ws });
        }
        Ok(out)
    };
    run().map_err(|e| format!("OCR 失敗: {e}"))
}
