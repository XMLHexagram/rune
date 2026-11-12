//! Types of Rune's OpenSubsonic extensions `runeService`, `runeCatalog` and
//! `runeRecommendations`, version 1.

use std::fmt;

use serde::{Deserialize, Deserializer};

use crate::{OneOrMany, SubsonicAlbum, SubsonicPlaylist};

/// A way to sign in to a service. Values this client does not know become `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuneLoginMethod {
    Password,
    Platform,
    Browser,
    DeviceCode,
    #[serde(other)]
    Other,
}

impl RuneLoginMethod {
    /// The wire name, or `None` for a method this client does not know.
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Password => Some("password"),
            Self::Platform => Some("platform"),
            Self::Browser => Some("browser"),
            Self::DeviceCode => Some("deviceCode"),
            Self::Other => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct RuneAttribution {
    #[serde(default)]
    pub text: String,
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct RuneLoginMethods {
    #[serde(default)]
    pub methods: Vec<RuneLoginMethod>,
}

/// An endpoint, or one parameter of it, that the service cannot provide.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RuneUnsupported {
    pub endpoint: String,
    #[serde(default)]
    pub param: Option<String>,
}

/// What `getRuneService` says about the service behind the server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct RuneServiceDescriptor {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub attribution: Option<RuneAttribution>,
    #[serde(default)]
    pub login: RuneLoginMethods,
    /// Platform host families that can authorize and play for this service.
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub unsupported: Vec<RuneUnsupported>,
}

impl RuneServiceDescriptor {
    /// Whether a call to `endpoint` that sends `params` is listed as unsupported: a
    /// whole-endpoint entry covers every call, a parameter entry only calls that send it.
    pub fn is_unsupported(&self, endpoint: &str, params: &[&str]) -> bool {
        self.unsupported.iter().any(|entry| {
            entry.endpoint == endpoint
                && entry
                    .param
                    .as_deref()
                    .is_none_or(|param| params.contains(&param))
        })
    }
}

/// What `startRuneLogin` asks the client to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuneLoginStart {
    Platform,
    Browser {
        url: String,
        state: String,
    },
    DeviceCode {
        user_code: String,
        verification_url: String,
        interval_secs: u32,
    },
    /// A method this client does not know.
    Other,
}

impl<'de> Deserialize<'de> for RuneLoginStart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            method: RuneLoginMethod,
            url: Option<String>,
            state: Option<String>,
            user_code: Option<String>,
            verification_url: Option<String>,
            interval: Option<u32>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let missing = |field: &'static str| <D::Error as serde::de::Error>::missing_field(field);
        Ok(match raw.method {
            RuneLoginMethod::Platform => Self::Platform,
            RuneLoginMethod::Browser => Self::Browser {
                url: raw.url.ok_or_else(|| missing("url"))?,
                state: raw.state.ok_or_else(|| missing("state"))?,
            },
            RuneLoginMethod::DeviceCode => Self::DeviceCode {
                user_code: raw.user_code.ok_or_else(|| missing("userCode"))?,
                verification_url: raw
                    .verification_url
                    .ok_or_else(|| missing("verificationUrl"))?,
                interval_secs: raw.interval.unwrap_or(5),
            },
            RuneLoginMethod::Password | RuneLoginMethod::Other => Self::Other,
        })
    }
}

/// The fields `completeRuneLogin` takes. Only the ones that apply to the method are set.
#[derive(Clone, PartialEq, Eq)]
pub struct RuneLoginCompletion {
    pub method: RuneLoginMethod,
    pub state: Option<String>,
    pub code: Option<String>,
    pub token: Option<String>,
    pub developer_token: Option<String>,
}

impl RuneLoginCompletion {
    pub(crate) fn form(&self) -> Option<Vec<(&'static str, &str)>> {
        let mut form = vec![("method", self.method.as_str()?)];
        for (key, value) in [
            ("state", &self.state),
            ("code", &self.code),
            ("token", &self.token),
            ("developerToken", &self.developer_token),
        ] {
            if let Some(value) = value {
                form.push((key, value.as_str()));
            }
        }
        Some(form)
    }
}

impl fmt::Debug for RuneLoginCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted = |value: &Option<String>| value.as_ref().map(|_| "[REDACTED]");
        formatter
            .debug_struct("RuneLoginCompletion")
            .field("method", &self.method)
            .field("state", &redacted(&self.state))
            .field("code", &redacted(&self.code))
            .field("token", &redacted(&self.token))
            .field("developer_token", &redacted(&self.developer_token))
            .finish()
    }
}

/// Why a service refused a request, beyond the Subsonic error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuneReason {
    CredentialsExpired,
    NotSubscribed,
    Region,
    Removed,
    ServiceLimit,
    #[serde(other)]
    Other,
}

/// How far a playlist can be edited. An unknown value allows everything, so the server decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuneEdit {
    Full,
    AppendOnly,
    ReadOnly,
    #[serde(other)]
    Unknown,
}

/// What a `runeCatalog` search covers. Servers without the extension search the library.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuneScope {
    /// The user's library only, as plain `search3`.
    #[default]
    Library,
    /// The service's catalogue only.
    Catalog,
    /// Library matches first, then catalogue matches that are not in the library.
    All,
}

impl RuneScope {
    /// The `runeScope` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Catalog => "catalog",
            Self::All => "all",
        }
    }

    /// The scope a `runeScope` value names, or `None` for a value this client does not know.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "library" => Some(Self::Library),
            "catalog" => Some(Self::Catalog),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// What a recommendation group is about. Values this client does not know become `Other`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuneRecommendationKind {
    Personal,
    Recent,
    Genre,
    #[default]
    #[serde(other)]
    Other,
}

impl RuneRecommendationKind {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Recent => "recent",
            Self::Genre => "genre",
            Self::Other => "other",
        }
    }
}

/// One titled group of `getRuneRecommendations`. Its albums and playlists open with
/// `getAlbum` and `getPlaylist`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuneRecommendationGroup {
    pub id: String,
    pub title: String,
    pub kind: RuneRecommendationKind,
    pub albums: Vec<SubsonicAlbum>,
    pub playlists: Vec<SubsonicPlaylist>,
}

impl<'de> Deserialize<'de> for RuneRecommendationGroup {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            id: String,
            #[serde(default)]
            title: String,
            #[serde(default)]
            kind: RuneRecommendationKind,
            #[serde(default)]
            album: Option<OneOrMany<SubsonicAlbum>>,
            #[serde(default)]
            playlist: Option<OneOrMany<SubsonicPlaylist>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            id: raw.id,
            title: raw.title,
            kind: raw.kind,
            albums: raw.album.map(OneOrMany::into_vec).unwrap_or_default(),
            playlists: raw.playlist.map(OneOrMany::into_vec).unwrap_or_default(),
        })
    }
}
