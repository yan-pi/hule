#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Minimal,
    Developer,
}

impl Preset {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "minimal" => Some(Self::Minimal),
            "developer" => Some(Self::Developer),
            _ => None,
        }
    }

    pub fn to_toml(self) -> String {
        let categories = match self {
            Self::Minimal => "[]",
            Self::Developer => "[\"rust_target\"]",
        };
        format!(
            "schema_version = 2\ndefault_profile = \"normal\"\nadditional_keep_paths = []\n\n[profiles.low]\ncategories = {categories}\n\n[profiles.normal]\ncategories = {categories}\n\n[profiles.aggressive]\ncategories = {categories}\n"
        )
    }
}
