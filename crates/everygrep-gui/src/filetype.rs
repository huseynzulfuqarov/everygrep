//! File kind from a file name, used to pick a row icon and the type filter.
//!
//! The classification looks only at the name (no disk access), so it is cheap
//! enough to run for every visible row. Symbolic links are detected by the
//! caller from file metadata and shown as [`FileKind::Link`].

/// Broad file category. Each kind has one icon in `ui/icons/types/`.
///
/// The discriminant is the index of the kind's icon in `Icons.types` in
/// `ui/app.slint`; the order there must match (a test checks it).
#[allow(dead_code)] // `Link` is set by the caller from metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Folder = 0,
    File,
    Text,
    Document,
    Pdf,
    Spreadsheet,
    Presentation,
    Image,
    Audio,
    Video,
    Archive,
    Code,
    Executable,
    Font,
    DiskImage,
    Link,
}

impl FileKind {
    /// Index of the kind's icon in the window's icon list.
    pub fn icon_index(self) -> i32 {
        self as i32
    }

    /// Icon file name (without `.svg`) in `ui/icons/types/`.
    #[allow(dead_code)]
    pub fn icon_name(self) -> &'static str {
        match self {
            FileKind::Folder => "folder",
            FileKind::File => "file",
            FileKind::Text => "text",
            FileKind::Document => "document",
            FileKind::Pdf => "pdf",
            FileKind::Spreadsheet => "spreadsheet",
            FileKind::Presentation => "presentation",
            FileKind::Image => "image",
            FileKind::Audio => "audio",
            FileKind::Video => "video",
            FileKind::Archive => "archive",
            FileKind::Code => "code",
            FileKind::Executable => "executable",
            FileKind::Font => "font",
            FileKind::DiskImage => "disk-image",
            FileKind::Link => "link",
        }
    }
}

/// Longest extension we look up, in bytes. Longer suffixes are not extensions.
const MAX_EXT: usize = 12;

/// Compound extensions checked before the last extension.
const COMPOUND: &[(&str, FileKind)] = &[
    ("tar.gz", FileKind::Archive),
    ("tar.bz2", FileKind::Archive),
    ("tar.xz", FileKind::Archive),
    ("tar.zst", FileKind::Archive),
    ("tar.lz", FileKind::Archive),
    ("tar.lz4", FileKind::Archive),
    ("tar.lzma", FileKind::Archive),
    ("tar.z", FileKind::Archive),
    ("pkg.tar.zst", FileKind::Archive),
    ("pkg.tar.xz", FileKind::Archive),
    ("d.ts", FileKind::Code),
];

/// Classifies an entry by its name. `name` is the last path component as raw
/// bytes (Linux file names need not be UTF-8).
pub fn classify(name: &[u8], is_dir: bool) -> FileKind {
    if is_dir {
        return FileKind::Folder;
    }
    // A leading dot marks a hidden file, not an extension: ".bashrc" has none,
    // ".config.toml" has "toml".
    let stem_start = name.iter().position(|&b| b != b'.').unwrap_or(name.len());
    let body = &name[stem_start..];

    if let Some(kind) = by_whole_name(body) {
        return kind;
    }

    let lower = body.to_ascii_lowercase();
    for &(ext, kind) in COMPOUND {
        if has_suffix_ext(&lower, ext.as_bytes()) {
            return kind;
        }
    }
    match lower.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 && lower.len() - dot - 1 <= MAX_EXT => {
            by_extension(&lower[dot + 1..]).unwrap_or(FileKind::File)
        }
        _ => FileKind::File,
    }
}

/// True when `name` ends with `.ext` and has a non-empty stem before it.
fn has_suffix_ext(name: &[u8], ext: &[u8]) -> bool {
    name.len() > ext.len() + 1 && name.ends_with(ext) && name[name.len() - ext.len() - 1] == b'.'
}

/// Well-known files without a telling extension.
fn by_whole_name(name: &[u8]) -> Option<FileKind> {
    let kind = match name {
        b"Makefile" | b"makefile" | b"GNUmakefile" | b"Dockerfile" | b"Containerfile"
        | b"PKGBUILD" | b"CMakeLists.txt" | b"Justfile" | b"justfile" | b"meson.build"
        | b"Cargo.lock" | b"bashrc" | b"zshrc" | b"profile" | b"gitignore" | b"gitattributes"
        | b"editorconfig" => FileKind::Code,
        b"README" | b"LICENSE" | b"COPYING" | b"AUTHORS" | b"CHANGELOG" | b"NEWS" | b"TODO" => {
            FileKind::Text
        }
        _ => return None,
    };
    Some(kind)
}

/// Kind for a lowercase extension without the dot.
fn by_extension(ext: &[u8]) -> Option<FileKind> {
    let ext = std::str::from_utf8(ext).ok()?;
    let kind = match ext {
        // Plain text and notes
        "txt" | "text" | "md" | "markdown" | "rst" | "org" | "adoc" | "log" | "nfo" | "srt"
        | "vtt" | "tex" | "bib" => FileKind::Text,

        // Word processing
        "doc" | "docx" | "docm" | "dot" | "dotx" | "odt" | "ott" | "fodt" | "rtf" | "wpd"
        | "pages" | "abw" | "epub" | "mobi" | "azw3" | "djvu" | "xps" | "oxps" => {
            FileKind::Document
        }

        "pdf" => FileKind::Pdf,

        "xls" | "xlsx" | "xlsm" | "xlsb" | "xlt" | "xltx" | "ods" | "ots" | "fods" | "csv"
        | "tsv" | "numbers" => FileKind::Spreadsheet,

        "ppt" | "pptx" | "pptm" | "pps" | "ppsx" | "pot" | "potx" | "odp" | "otp" | "fodp"
        | "key" => FileKind::Presentation,

        "png" | "jpg" | "jpeg" | "jpe" | "jfif" | "gif" | "bmp" | "webp" | "avif" | "heic"
        | "heif" | "jxl" | "tif" | "tiff" | "svg" | "svgz" | "ico" | "icns" | "xcf" | "psd"
        | "kra" | "ora" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "tga"
        | "qoi" | "exr" | "hdr" | "pbm" | "pgm" | "ppm" | "pnm" | "xpm" | "eps" | "ai" => {
            FileKind::Image
        }

        "mp3" | "flac" | "ogg" | "oga" | "opus" | "wav" | "wave" | "aac" | "m4a" | "m4b"
        | "wma" | "aiff" | "aif" | "alac" | "ape" | "wv" | "mid" | "midi" | "mka" | "spx"
        | "ac3" | "dts" | "amr" => FileKind::Audio,

        "mp4" | "m4v" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "flv" | "mpg" | "mpeg"
        | "m2ts" | "mts" | "3gp" | "3g2" | "ogv" | "vob" | "rmvb" | "f4v" => FileKind::Video,

        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "tbz" | "tbz2" | "xz" | "txz"
        | "zst" | "tzst" | "lz" | "lz4" | "lzma" | "z" | "cab" | "arj" | "lzh" | "cpio" | "ar"
        | "deb" | "rpm" | "apk" | "jar" | "war" | "whl" | "crate" | "gem" | "xpi" | "snap"
        | "flatpak" => FileKind::Archive,

        "rs" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "cs" | "java" | "kt"
        | "kts" | "scala" | "go" | "py" | "pyi" | "rb" | "php" | "pl" | "pm" | "lua" | "js"
        | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "vue" | "svelte" | "swift" | "m" | "mm"
        | "dart" | "zig" | "nim" | "hs" | "ml" | "mli" | "ex" | "exs" | "erl" | "clj" | "el"
        | "lisp" | "scm" | "r" | "jl" | "sql" | "html" | "htm" | "xhtml" | "css" | "scss"
        | "sass" | "less" | "json" | "jsonc" | "json5" | "yaml" | "yml" | "toml" | "ini"
        | "cfg" | "conf" | "xml" | "xsd" | "xsl" | "slint" | "qml" | "ui" | "glsl" | "wgsl"
        | "hlsl" | "proto" | "graphql" | "gradle" | "cmake" | "mk" | "nix" | "patch" | "diff"
        | "fish" | "zsh" | "ps1" | "bat" | "cmd" | "asm" | "s" | "v" | "vhdl" | "sv" | "ipynb"
        | "service" | "timer" | "socket" | "rules" | "desktop" => FileKind::Code,

        // Things that run when opened: scripts with a shebang by convention,
        // self-contained Linux apps, Windows programs, shared libraries.
        "sh" | "bash" | "run" | "bin" | "appimage" | "exe" | "msi" | "com" | "elf" | "out"
        | "so" | "dll" => FileKind::Executable,

        "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2" | "pfb" | "pfa" | "bdf" | "pcf" => {
            FileKind::Font
        }

        "iso" | "img" | "dmg" | "vhd" | "vhdx" | "vmdk" | "qcow" | "qcow2" | "vdi" | "toast"
        | "nrg" | "mdf" | "cue" | "ima" => FileKind::DiskImage,

        _ => return None,
    };
    Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(name: &str) -> FileKind {
        classify(name.as_bytes(), false)
    }

    #[test]
    fn directories_are_folders_regardless_of_name() {
        assert_eq!(classify(b"photos.jpg", true), FileKind::Folder);
        assert_eq!(classify(b"src", true), FileKind::Folder);
    }

    #[test]
    fn no_extension_is_generic_file() {
        assert_eq!(k("notes"), FileKind::File);
        assert_eq!(k(""), FileKind::File);
        assert_eq!(k("trailing."), FileKind::File);
        assert_eq!(k("..."), FileKind::File);
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert_eq!(k("IMG_0001.JPG"), FileKind::Image);
        assert_eq!(k("Report.PdF"), FileKind::Pdf);
        assert_eq!(k("Tool.AppImage"), FileKind::Executable);
    }

    #[test]
    fn compound_extensions() {
        assert_eq!(k("linux-7.3.tar.gz"), FileKind::Archive);
        assert_eq!(k("backup.TAR.ZST"), FileKind::Archive);
        assert_eq!(k("mesa-26.0-1-x86_64.pkg.tar.zst"), FileKind::Archive);
        assert_eq!(k("types.d.ts"), FileKind::Code);
        // Version numbers are not extensions.
        assert_eq!(k("libfoo.so.1.2"), FileKind::File);
    }

    #[test]
    fn hidden_files() {
        assert_eq!(k(".bashrc"), FileKind::Code);
        assert_eq!(k(".hidden"), FileKind::File);
        assert_eq!(k(".config.toml"), FileKind::Code);
        assert_eq!(k(".mp3"), FileKind::File);
    }

    #[test]
    fn whole_names() {
        assert_eq!(k("Makefile"), FileKind::Code);
        assert_eq!(k("PKGBUILD"), FileKind::Code);
        assert_eq!(k("README"), FileKind::Text);
        assert_eq!(k("README.md"), FileKind::Text);
    }

    #[test]
    fn one_example_per_kind() {
        let cases = [
            ("a.txt", FileKind::Text),
            ("a.odt", FileKind::Document),
            ("a.docx", FileKind::Document),
            ("a.pdf", FileKind::Pdf),
            ("a.xlsx", FileKind::Spreadsheet),
            ("a.csv", FileKind::Spreadsheet),
            ("a.pptx", FileKind::Presentation),
            ("a.webp", FileKind::Image),
            ("a.flac", FileKind::Audio),
            ("a.mkv", FileKind::Video),
            ("a.7z", FileKind::Archive),
            ("a.rs", FileKind::Code),
            ("a.desktop", FileKind::Code),
            ("install.sh", FileKind::Executable),
            ("setup.exe", FileKind::Executable),
            ("a.woff2", FileKind::Font),
            ("cachyos.iso", FileKind::DiskImage),
            ("disk.qcow2", FileKind::DiskImage),
        ];
        for (name, kind) in cases {
            assert_eq!(k(name), kind, "{name}");
        }
    }

    #[test]
    fn non_utf8_names_do_not_panic() {
        assert_eq!(classify(b"\xff\xfe.png", false), FileKind::Image);
        assert_eq!(classify(b"x.\xff\xfe", false), FileKind::File);
    }

    #[test]
    fn very_long_suffix_is_not_an_extension() {
        assert_eq!(k("archive.abcdefghijklmnopq"), FileKind::File);
    }

    const ALL: [FileKind; 16] = [
        FileKind::Folder,
        FileKind::File,
        FileKind::Text,
        FileKind::Document,
        FileKind::Pdf,
        FileKind::Spreadsheet,
        FileKind::Presentation,
        FileKind::Image,
        FileKind::Audio,
        FileKind::Video,
        FileKind::Archive,
        FileKind::Code,
        FileKind::Executable,
        FileKind::Font,
        FileKind::DiskImage,
        FileKind::Link,
    ];

    #[test]
    fn every_kind_has_an_icon_file() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/icons/types");
        for kind in ALL {
            let path = format!("{dir}/{}.svg", kind.icon_name());
            assert!(std::path::Path::new(&path).is_file(), "{path}");
        }
    }

    #[test]
    fn window_icon_list_follows_the_kind_order() {
        let ui = include_str!("../ui/app.slint");
        let start = ui.find("out property <[image]> types").expect("icon list");
        let list = &ui[start..start + ui[start..].find("];").expect("list end")];
        let names: Vec<&str> = list
            .split("@image-url(\"icons/types/")
            .skip(1)
            .map(|s| &s[..s.find(".svg").expect("svg")])
            .collect();
        let expected: Vec<&str> = ALL.iter().map(|k| k.icon_name()).collect();
        assert_eq!(names, expected);
        for (i, kind) in ALL.iter().enumerate() {
            assert_eq!(kind.icon_index(), i as i32);
        }
    }
}
