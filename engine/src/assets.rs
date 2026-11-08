use std::path::Path;

use oak_core::{detect_engine, GameEngine};
use serde_json::{json, Value};

use crate::args::Args;
use crate::error::EngineError;

/// Binary formats common to every engine's source assets.
const COMMON_BINARY: &[&str] = &[
    "png", "jpg", "jpeg", "tga", "bmp", "psd", "tif", "tiff", "exr", "hdr", "webp", "dds", "ktx",
    "wav", "ogg", "mp3", "flac", "aif", "aiff", "fbx", "blend", "obj", "dae", "glb", "gltf", "abc",
    "ttf", "otf", "woff", "woff2", "mp4", "mov", "webm", "ogv", "zip", "pdf",
];

/// Formats Godot imports into `.godot/`, each of which gets a `.import` sidecar.
const GODOT_IMPORTED: &[&str] = &[
    "png", "jpg", "jpeg", "tga", "bmp", "psd", "exr", "hdr", "webp", "dds", "ktx", "svg", "wav",
    "ogg", "mp3", "fbx", "blend", "obj", "dae", "glb", "gltf", "ttf", "otf", "woff", "woff2",
    "csv",
];
/// Godot's own binary resources.
const GODOT_BINARY: &[&str] = &["res", "scn", "mesh", "anim", "ctex", "tex"];
/// Formats that can't hold a UID themselves, so Godot 4.4+ writes `.uid` beside them.
const GODOT_UID: &[&str] = &["gd", "cs", "gdshader", "gdshaderinc", "gdextension"];

/// Unity's own asset formats, from its supported-types page (NativeFormatImporter,
/// PrefabImporter, VisualEffectImporter) plus scenes and later additions, in
/// lower case. Text only when the project has Force Text serialization.
const UNITY_YAML: &[&str] = &[
    "unity",
    "prefab",
    "anim",
    "asset",
    "blendtree",
    "brush",
    "buildreport",
    "colors",
    "controller",
    "cubemap",
    "curves",
    "curvesnormalized",
    "flare",
    "fontsettings",
    "giparams",
    "gradients",
    "guiskin",
    "ht",
    "lighting",
    "mask",
    "mat",
    "mesh",
    "mixer",
    "overridecontroller",
    "particlecurves",
    "particlecurvessigned",
    "particledoublecurves",
    "particledoublecurvessigned",
    "physicmaterial",
    "physicsmaterial2d",
    "playable",
    "preset",
    "rendertexture",
    "shadervariants",
    "signal",
    "spriteatlas",
    "spriteatlasv2",
    "state",
    "statemachine",
    "terrainlayer",
    "texture2d",
    "transition",
    "vfx",
    "vfxblock",
    "vfxoperator",
    "webcamtexture",
];
const UNITY_BINARY: &[&str] = &["dll", "so", "dylib", "a", "unitypackage", "bundle"];

const UNREAL_BINARY: &[&str] = &["uasset", "umap", "ubulk", "uexp", "upk", "dll", "pdb"];

fn engine_name(engine: GameEngine) -> &'static str {
    match engine {
        GameEngine::Godot => "godot",
        GameEngine::Unity => "unity",
        GameEngine::Unreal => "unreal",
    }
}

pub fn detect(args: &Args) -> Result<Value, EngineError> {
    let engine = detect_engine(Path::new(args.str("path")?));
    Ok(json!({ "engine": engine.map(engine_name) }))
}

pub fn ignore_template(args: &Args) -> Result<Value, EngineError> {
    Ok(json!({ "contents": args.engine()?.oakignore_contents() }))
}

fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension.to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Class {
    Text,
    Binary,
    Sidecar,
}

fn class_of(engine: GameEngine, path: &str, force_text: bool) -> Class {
    let extension = extension(path);
    let extension = extension.as_str();
    let binary = match engine {
        GameEngine::Godot => {
            if matches!(extension, "import" | "uid") {
                return Class::Sidecar;
            }
            GODOT_BINARY.contains(&extension)
        }
        GameEngine::Unity => {
            if extension == "meta" {
                return Class::Sidecar;
            }
            if UNITY_YAML.contains(&extension) {
                return if force_text {
                    Class::Text
                } else {
                    Class::Binary
                };
            }
            UNITY_BINARY.contains(&extension)
        }
        GameEngine::Unreal => UNREAL_BINARY.contains(&extension),
    };
    if binary || COMMON_BINARY.contains(&extension) {
        Class::Binary
    } else {
        Class::Text
    }
}

/// `assets.classify`. A format this table doesn't know is `text`: Oak's own
/// binary detection still protects the file. `unity_force_text` is optional and off
/// by default, so an unknown project gets the safe answer for YAML assets.
pub fn classify(args: &Args) -> Result<Value, EngineError> {
    let engine = args.engine()?;
    let force_text = args.bool("unity_force_text")?;
    let assets: Vec<Value> = args
        .str_list("paths")?
        .into_iter()
        .map(|path| {
            let path = path.replace('\\', "/");
            let (kind, merge) = match class_of(engine, &path, force_text) {
                Class::Text => ("text", "text"),
                Class::Binary => ("binary", "none"),
                Class::Sidecar => ("sidecar", "text"),
            };
            json!({"path": path, "kind": kind, "merge": merge})
        })
        .collect();
    Ok(json!({ "assets": assets }))
}

/// The sidecar a path may have, or the asset a sidecar belongs to.
fn companions_of(engine: GameEngine, path: &str) -> Vec<String> {
    let path = path.trim_end_matches('/');
    match engine {
        GameEngine::Godot => {
            if let Some(asset) = path.strip_suffix(".import").or(path.strip_suffix(".uid")) {
                return vec![asset.to_string()];
            }
            let extension = extension(path);
            let mut sidecars = Vec::new();
            if GODOT_IMPORTED.contains(&extension.as_str()) {
                sidecars.push(format!("{path}.import"));
            }
            if GODOT_UID.contains(&extension.as_str()) {
                sidecars.push(format!("{path}.uid"));
            }
            sidecars
        }
        GameEngine::Unity => {
            if let Some(asset) = path.strip_suffix(".meta") {
                return vec![asset.to_string()];
            }
            // Folders have `.meta` files too. ProjectSettings/ has none.
            if path
                .split('/')
                .any(|part| part == "Assets" || part == "Packages")
                && !matches!(path.rsplit('/').next(), Some("Assets" | "Packages"))
            {
                vec![format!("{path}.meta")]
            } else {
                Vec::new()
            }
        }
        GameEngine::Unreal => Vec::new(),
    }
}

/// `assets.companions`: the paths expanded with their sidecars, in input order
/// with each path's companions right after it. No filesystem access: a
/// companion may not exist, and the extension skips those that don't.
pub fn companions(args: &Args) -> Result<Value, EngineError> {
    let engine = args.engine()?;
    let mut paths: Vec<String> = Vec::new();
    let mut push = |path: String| {
        if !paths.contains(&path) {
            paths.push(path);
        }
    };
    for path in args.str_list("paths")? {
        let path = path.replace('\\', "/");
        let companions = companions_of(engine, &path);
        push(path.trim_end_matches('/').to_string());
        companions.into_iter().for_each(&mut push);
    }
    Ok(json!({ "paths": paths }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(op: fn(&Args) -> Result<Value, EngineError>, args: Value) -> Result<Value, EngineError> {
        op(&Args::new(&args).unwrap())
    }

    fn is(engine: &str, path: &str, kind: &str, merge: &str) {
        let result = run(classify, json!({"engine": engine, "paths": [path]})).unwrap();
        let asset = &result["assets"][0];
        assert_eq!(
            (asset["kind"].as_str(), asset["merge"].as_str()),
            (Some(kind), Some(merge)),
            "{engine} {path}"
        );
    }

    #[test]
    fn classifies_godot() {
        is("godot", "icon.png.import", "sidecar", "text");
        is("godot", "player.gd.uid", "sidecar", "text");
        is("godot", "level.scn", "binary", "none");
        is("godot", "art/hero.PNG", "binary", "none");
        is("godot", "level.tscn", "text", "text");
        is("godot", "player.gd", "text", "text");
    }

    #[test]
    fn classifies_unity() {
        is("unity", "Assets/a.png.meta", "sidecar", "text");
        is("unity", "Assets/a.png", "binary", "none");
        is("unity", "Assets/a.dll", "binary", "none");
        is("unity", "Assets/a.cs", "text", "text");
        is("unity", "Assets/Main.unity", "binary", "none");
        is("unity", "Assets/Hit.overrideController", "binary", "none");
        let result = run(
            classify,
            json!({"engine": "unity", "paths": ["Assets\\Main.unity"], "unity_force_text": true}),
        )
        .unwrap();
        assert_eq!(
            result["assets"][0],
            json!({"path": "Assets/Main.unity", "kind": "text", "merge": "text"})
        );
    }

    #[test]
    fn classifies_unreal_and_oddities() {
        is("unreal", "Content/Hero.uasset", "binary", "none");
        is("unreal", "Content/Map.umap", "binary", "none");
        is("unreal", "Source/Game.cpp", "text", "text");
        is("unreal", "Hero.png.import", "text", "text");
        is("unreal", "README", "text", "text");
        is("unreal", ".gitattributes", "text", "text");
    }

    fn expand(engine: &str, paths: Value) -> Value {
        run(companions, json!({"engine": engine, "paths": paths})).unwrap()["paths"].clone()
    }

    #[test]
    fn godot_companions_go_both_ways() {
        assert_eq!(
            expand("godot", json!(["a.png", "b.gd", "c.tscn"])),
            json!(["a.png", "a.png.import", "b.gd", "b.gd.uid", "c.tscn"])
        );
        assert_eq!(
            expand("godot", json!(["a.png.import"])),
            json!(["a.png.import", "a.png"])
        );
        assert_eq!(
            expand("godot", json!(["b.gd.uid", "b.gd"])),
            json!(["b.gd.uid", "b.gd"])
        );
    }

    #[test]
    fn unity_companions_cover_folders_only_under_assets() {
        assert_eq!(
            expand("unity", json!(["game/Assets/Art/", "Assets/a.png.meta"])),
            json!([
                "game/Assets/Art",
                "game/Assets/Art.meta",
                "Assets/a.png.meta",
                "Assets/a.png"
            ])
        );
        assert_eq!(
            expand("unity", json!(["ProjectSettings/x.asset"])),
            json!(["ProjectSettings/x.asset"])
        );
        assert_eq!(expand("unity", json!(["Assets"])), json!(["Assets"]));
    }

    #[test]
    fn unreal_has_no_sidecars() {
        assert_eq!(
            expand("unreal", json!(["Content/a.uasset"])),
            json!(["Content/a.uasset"])
        );
    }

    #[test]
    fn detect_and_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        assert_eq!(
            run(detect, json!({"path": path})).unwrap()["engine"],
            Value::Null
        );
        std::fs::write(dir.path().join("project.godot"), "").unwrap();
        assert_eq!(
            run(detect, json!({"path": path})).unwrap()["engine"],
            "godot"
        );

        for engine in ["godot", "unity", "unreal"] {
            let contents = run(ignore_template, json!({"engine": engine})).unwrap();
            assert!(contents["contents"].as_str().unwrap().contains('#'));
        }
        assert!(run(ignore_template, json!({"engine": "cryengine"})).is_err());
        assert!(run(classify, json!({"engine": "unity", "paths": [1]})).is_err());
        assert!(run(detect, json!({})).is_err());
    }
}
