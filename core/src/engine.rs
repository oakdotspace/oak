use std::fmt;
use std::path::Path;

/// Supported game engines that Oak can detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameEngine {
    Godot,
    Unity,
    Unreal,
}

impl fmt::Display for GameEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GameEngine::Godot => write!(f, "Godot"),
            GameEngine::Unity => write!(f, "Unity"),
            GameEngine::Unreal => write!(f, "Unreal Engine"),
        }
    }
}

impl GameEngine {
    /// Returns the recommended `.oakignore` contents for this engine.
    pub fn oakignore_contents(&self) -> &'static str {
        match self {
            GameEngine::Godot => GODOT_OAKIGNORE,
            GameEngine::Unity => UNITY_OAKIGNORE,
            GameEngine::Unreal => UNREAL_OAKIGNORE,
        }
    }
}

/// Detect which game engine (if any) is used in the given project directory.
pub fn detect_engine(path: &Path) -> Option<GameEngine> {
    // Godot: project.godot file
    if path.join("project.godot").exists() {
        return Some(GameEngine::Godot);
    }

    // Unity: ProjectSettings/ + Assets/ directories
    if path.join("ProjectSettings").is_dir() && path.join("Assets").is_dir() {
        return Some(GameEngine::Unity);
    }

    // Unreal: any *.uproject file in root
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Some(ext) = entry.path().extension() {
                if ext == "uproject" {
                    return Some(GameEngine::Unreal);
                }
            }
        }
    }

    None
}

const GODOT_OAKIGNORE: &str = "\
# Godot
.godot/
*.import
";

const UNITY_OAKIGNORE: &str = "\
# Unity
Library/
Temp/
Logs/
UserSettings/
obj/
Builds/
Build/
*.csproj
*.sln
";

const UNREAL_OAKIGNORE: &str = "\
# Unreal Engine
Intermediate/
Saved/
Binaries/
DerivedDataCache/
.vs/
*.sln
*.vcxproj
*.vcxproj.filters
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_detect_godot() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("project.godot"), "").unwrap();
        assert_eq!(detect_engine(temp.path()), Some(GameEngine::Godot));
    }

    #[test]
    fn test_detect_unity() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join("ProjectSettings")).unwrap();
        fs::create_dir(temp.path().join("Assets")).unwrap();
        assert_eq!(detect_engine(temp.path()), Some(GameEngine::Unity));
    }

    #[test]
    fn test_detect_unity_missing_assets() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join("ProjectSettings")).unwrap();
        assert_eq!(detect_engine(temp.path()), None);
    }

    #[test]
    fn test_detect_unreal() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("MyGame.uproject"), "{}").unwrap();
        assert_eq!(detect_engine(temp.path()), Some(GameEngine::Unreal));
    }

    #[test]
    fn test_detect_none() {
        let temp = TempDir::new().unwrap();
        assert_eq!(detect_engine(temp.path()), None);
    }

    #[test]
    fn test_oakignore_contents_not_empty() {
        assert!(!GameEngine::Godot.oakignore_contents().is_empty());
        assert!(!GameEngine::Unity.oakignore_contents().is_empty());
        assert!(!GameEngine::Unreal.oakignore_contents().is_empty());
    }

    #[test]
    fn test_display() {
        assert_eq!(GameEngine::Godot.to_string(), "Godot");
        assert_eq!(GameEngine::Unity.to_string(), "Unity");
        assert_eq!(GameEngine::Unreal.to_string(), "Unreal Engine");
    }
}
