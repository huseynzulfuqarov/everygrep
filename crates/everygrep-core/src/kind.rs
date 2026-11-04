//! File kind from a file name, used for row icons, the Type column and result
//! filters.
//!
//! The classification looks only at the name (no disk access) and does not
//! allocate, so it is cheap enough to run for every node of a search.
//! Symbolic links are not known from the name; callers that read file
//! metadata can show them as [`FileKind::Link`].

/// Broad file category.
///
/// The order is part of the API: the window keeps its icons in this order, and
/// sorting by type groups results by kind in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// Never returned by [`classify`]; set by callers from file metadata.
    Link,
}

impl FileKind {
    /// Every kind, in order.
    pub const ALL: [FileKind; 16] = [
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
    /// Stable query name, independent of translations and icon paths.
    pub fn id(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::File => "file",
            Self::Text => "text",
            Self::Document => "document",
            Self::Pdf => "pdf",
            Self::Spreadsheet => "spreadsheet",
            Self::Presentation => "presentation",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Archive => "archive",
            Self::Code => "code",
            Self::Executable => "executable",
            Self::Font => "font",
            Self::DiskImage => "disk-image",
            Self::Link => "link",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.id().eq_ignore_ascii_case(id))
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

/// Bytes of a name's end that are lowercased for the lookups; longer than any
/// compound extension plus its dot.
const TAIL: usize = 16;

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

    // Only the end of the name matters from here on; lowercase it on the stack.
    let start = body.len().saturating_sub(TAIL);
    let mut buf = [0u8; TAIL];
    let tail = &mut buf[..body.len() - start];
    tail.copy_from_slice(&body[start..]);
    tail.make_ascii_lowercase();
    let tail = &*tail;

    for &(ext, kind) in COMPOUND {
        let ext = ext.as_bytes();
        // The stem before the dot must not be empty.
        if body.len() > ext.len() + 1
            && tail.ends_with(ext)
            && tail[tail.len() - ext.len() - 1] == b'.'
        {
            return kind;
        }
    }
    match tail.iter().rposition(|&b| b == b'.') {
        // `start + dot > 0`: the dot is not the first byte of the body.
        Some(dot) if start + dot > 0 && tail.len() - dot - 1 <= MAX_EXT => {
            by_extension(&tail[dot + 1..]).unwrap_or(FileKind::File)
        }
        _ => FileKind::File,
    }
}

/// Well-known files without a telling extension.
fn by_whole_name(name: &[u8]) -> Option<FileKind> {
    let kind = match name {
        b"Makefile" | b"makefile" | b"GNUmakefile" | b"Dockerfile" | b"Containerfile"
        | b"PKGBUILD" | b"CMakeLists.txt" | b"Justfile" | b"justfile" | b"meson.build"
        | b"Cargo.lock" | b"bashrc" | b"zshrc" | b"profile" | b"gitignore" | b"gitattributes"
        | b"editorconfig" | b"go.mod" | b"go.sum" => FileKind::Code,
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

        // Word processing and e-books
        "doc" | "docx" | "docm" | "dot" | "dotx" | "dotm" | "odt" | "ott" | "fodt" | "rtf"
        | "wpd" | "wps" | "pages" | "abw" | "epub" | "mobi" | "azw3" | "djvu" | "xps" | "oxps"
        | "chm" | "mht" | "mhtml" | "vsd" => FileKind::Document,

        "pdf" => FileKind::Pdf,

        "xls" | "xlsx" | "xlsm" | "xlsb" | "xlt" | "xltx" | "xltm" | "ods" | "ots" | "fods"
        | "csv" | "tsv" | "numbers" => FileKind::Spreadsheet,

        "ppt" | "pptx" | "pptm" | "pps" | "ppsx" | "ppsm" | "pot" | "potx" | "potm" | "odp"
        | "otp" | "fodp" | "key" => FileKind::Presentation,

        "png" | "apng" | "jpg" | "jpeg" | "jpe" | "jfif" | "gif" | "bmp" | "webp" | "avif"
        | "heic" | "heif" | "jxl" | "tif" | "tiff" | "svg" | "svgz" | "ico" | "icns" | "xcf"
        | "psd" | "kra" | "ora" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2"
        | "tga" | "qoi" | "exr" | "hdr" | "pbm" | "pgm" | "ppm" | "pnm" | "xpm" | "eps" | "ai"
        | "avifs" | "cur" | "dds" | "hif" | "jxr" | "pcx" | "psb" | "wmf" => FileKind::Image,

        "mp3" | "flac" | "ogg" | "oga" | "opus" | "wav" | "wave" | "aac" | "m4a" | "m4b"
        | "wma" | "aiff" | "aif" | "alac" | "ape" | "wv" | "mid" | "midi" | "mka" | "spx"
        | "ac3" | "dts" | "amr" | "mpc" | "weba" | "m3u" | "m3u8" | "pls" | "cue" | "aifc"
        | "au" | "mp2" | "ra" | "snd" => FileKind::Audio,

        "mp4" | "m4v" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "flv" | "mpg" | "mpeg"
        | "m2ts" | "mts" | "3gp" | "3g2" | "ogv" | "vob" | "rmvb" | "f4v" | "asf" | "divx"
        | "qt" | "3gpp" | "m2t" | "m2v" | "mpe" | "ogm" | "rm" => FileKind::Video,

        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "tbz" | "tbz2" | "xz" | "txz"
        | "zst" | "tzst" | "lz" | "lz4" | "lzma" | "z" | "cab" | "arj" | "lzh" | "cpio" | "ar"
        | "deb" | "rpm" | "apk" | "jar" | "war" | "whl" | "crate" | "gem" | "xpi" | "snap"
        | "flatpak" => FileKind::Archive,
        // Parts of a split RAR archive.
        r if r.len() == 3
            && r.starts_with('r')
            && r[1..].parse::<u8>().is_ok_and(|n| n <= 29)
            && r.as_bytes()[1].is_ascii_digit() =>
        {
            FileKind::Archive
        }

        "rs" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "cs" | "java" | "kt"
        | "kts" | "scala" | "go" | "py" | "pyi" | "rb" | "php" | "pl" | "pm" | "lua" | "js"
        | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "vue" | "svelte" | "swift" | "m" | "mm"
        | "dart" | "zig" | "nim" | "hs" | "ml" | "mli" | "ex" | "exs" | "erl" | "clj" | "el"
        | "lisp" | "scm" | "r" | "jl" | "sql" | "html" | "htm" | "xhtml" | "css" | "scss"
        | "sass" | "less" | "json" | "jsonc" | "json5" | "yaml" | "yml" | "toml" | "ini"
        | "cfg" | "conf" | "xml" | "xsd" | "xsl" | "slint" | "qml" | "ui" | "glsl" | "wgsl"
        | "hlsl" | "proto" | "graphql" | "gradle" | "cmake" | "mk" | "nix" | "patch" | "diff"
        | "fish" | "zsh" | "asm" | "s" | "v" | "vhdl" | "sv" | "ipynb" | "service" | "timer"
        | "socket" | "rules" | "vb" => FileKind::Code,

        // Things that run when opened: scripts with a shebang by convention,
        // self-contained Linux apps, desktop launchers, Windows programs and
        // scripts, shared libraries.
        "sh" | "bash" | "run" | "bin" | "appimage" | "desktop" | "exe" | "msi" | "com" | "bat"
        | "cmd" | "ps1" | "vbs" | "elf" | "out" | "so" | "dll" => FileKind::Executable,

        "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2" | "pfb" | "pfa" | "bdf" | "pcf" => {
            FileKind::Font
        }

        "iso" | "img" | "dmg" | "vhd" | "vhdx" | "vmdk" | "qcow" | "qcow2" | "vdi" | "toast"
        | "nrg" | "mdf" | "ima" => FileKind::DiskImage,

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
        // The stem must not be empty.
        assert_eq!(k("x.tar.gz"), FileKind::Archive);
        // Version numbers are not extensions.
        assert_eq!(k("libfoo.so.1.2"), FileKind::File);
    }

    #[test]
    fn hidden_files() {
        assert_eq!(k(".bashrc"), FileKind::Code);
        assert_eq!(k(".hidden"), FileKind::File);
        assert_eq!(k(".config.toml"), FileKind::Code);
        assert_eq!(k(".mp3"), FileKind::File);
        assert_eq!(k("..mp3"), FileKind::File);
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
            ("a.desktop", FileKind::Executable),
            ("install.sh", FileKind::Executable),
            ("setup.exe", FileKind::Executable),
            ("a.woff2", FileKind::Font),
            ("installer.iso", FileKind::DiskImage),
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
    fn long_names_use_their_end() {
        let long = format!("{}.flac", "a".repeat(300));
        assert_eq!(k(&long), FileKind::Audio);
        let long = format!("{}.pkg.tar.zst", "b".repeat(300));
        assert_eq!(k(&long), FileKind::Archive);
    }

    #[test]
    fn very_long_suffix_is_not_an_extension() {
        assert_eq!(k("archive.abcdefghijklmnopq"), FileKind::File);
        assert_eq!(k("archive.abcdefghijklm"), FileKind::File);
    }

    #[test]
    fn all_lists_every_kind_in_order() {
        for (i, kind) in FileKind::ALL.iter().enumerate() {
            assert_eq!(*kind as usize, i);
        }
    }
}
