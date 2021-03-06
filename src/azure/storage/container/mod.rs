pub mod requests;
pub mod responses;

use azure::core::{
    enumerations,
    errors::{AzureError, TraversingError},
    headers::{BLOB_PUBLIC_ACCESS, HAS_IMMUTABILITY_POLICY, HAS_LEGAL_HOLD, LEASE_DURATION, LEASE_STATE, LEASE_STATUS, META_PREFIX},
    lease::{LeaseDuration, LeaseState, LeaseStatus},
    parsing::{cast_must, cast_optional, traverse, FromStringOptional},
    ClientRequired, ContainerNameRequired, COMPLETE_ENCODE_SET,
};
use chrono::{DateTime, Utc};
use http::request::Builder;
use http::HeaderMap;
use hyper::header;
use hyper::header::HeaderName;
use std::collections::HashMap;
use std::{fmt, str::FromStr};
use url::percent_encoding::utf8_percent_encode;
use xml::{Element, Xml};

create_enum!(PublicAccess, (None, "none"), (Container, "container"), (Blob, "blob"));

pub(crate) fn public_access_from_header(header_map: &HeaderMap) -> Result<PublicAccess, AzureError> {
    let pa = match header_map.get(BLOB_PUBLIC_ACCESS) {
        Some(pa) => PublicAccess::from_str(pa.to_str()?)?,
        None => PublicAccess::None,
    };
    Ok(pa)
}

pub trait PublicAccessSupport {
    type O;
    fn with_public_access(self, public_access: PublicAccess) -> Self::O;
}

pub trait PublicAccessRequired {
    fn public_access(&self) -> PublicAccess;

    fn add_header(&self, builder: &mut Builder) {
        if self.public_access() != PublicAccess::None {
            builder.header(BLOB_PUBLIC_ACCESS, self.public_access().as_ref());
        }
    }
}

#[derive(Debug, Clone)]
pub struct Container {
    pub name: String,
    pub last_modified: DateTime<Utc>,
    pub e_tag: String,
    pub lease_status: LeaseStatus,
    pub lease_state: LeaseState,
    pub lease_duration: Option<LeaseDuration>,
    pub public_access: PublicAccess,
    pub has_immutability_policy: bool,
    pub has_legal_hold: bool,
    pub metadata: HashMap<String, String>,
}

impl AsRef<str> for Container {
    fn as_ref(&self) -> &str {
        &self.name
    }
}

impl Container {
    pub fn new(name: &str) -> Container {
        Container {
            name: name.to_owned(),
            last_modified: Utc::now(),
            e_tag: "".to_owned(),
            lease_status: LeaseStatus::Unlocked,
            lease_state: LeaseState::Available,
            lease_duration: None,
            public_access: PublicAccess::None,
            has_immutability_policy: false,
            has_legal_hold: false,
            metadata: HashMap::new(),
        }
    }

    pub fn from_response(name: String, headers: &HeaderMap) -> Result<Container, AzureError> {
        let last_modified = match headers.get(header::LAST_MODIFIED) {
            Some(last_modified) => last_modified.to_str()?,
            None => {
                static LM: header::HeaderName = header::LAST_MODIFIED;
                return Err(AzureError::MissingHeaderError(LM.as_str().to_owned()));
            }
        };
        let last_modified = DateTime::parse_from_rfc2822(last_modified)?;
        let last_modified = DateTime::from_utc(last_modified.naive_utc(), Utc);

        let e_tag = match headers.get(header::ETAG) {
            Some(e_tag) => e_tag.to_str()?.to_owned(),
            None => {
                static ETAG: HeaderName = header::ETAG;
                return Err(AzureError::MissingHeaderError(ETAG.as_str().to_owned()));
            }
        };

        let lease_status = match headers.get(LEASE_STATUS) {
            Some(lease_status) => lease_status.to_str()?,
            None => return Err(AzureError::MissingHeaderError(LEASE_STATUS.to_owned())),
        };
        let lease_status = LeaseStatus::from_str(lease_status)?;

        let lease_state = match headers.get(LEASE_STATE) {
            Some(lease_state) => lease_state.to_str()?,
            None => return Err(AzureError::MissingHeaderError(LEASE_STATE.to_owned())),
        };
        let lease_state = LeaseState::from_str(lease_state)?;

        let lease_duration = match headers.get(LEASE_DURATION) {
            Some(lease_duration) => Some(LeaseDuration::from_str(lease_duration.to_str()?)?),
            None => None,
        };

        let public_access = public_access_from_header(&headers)?;

        let has_immutability_policy = match headers.get(HAS_IMMUTABILITY_POLICY) {
            Some(has_immutability_policy) => bool::from_str(has_immutability_policy.to_str()?)?,
            None => return Err(AzureError::MissingHeaderError(HAS_IMMUTABILITY_POLICY.to_owned())),
        };

        let has_legal_hold = match headers.get(HAS_LEGAL_HOLD) {
            Some(has_legal_hold) => bool::from_str(has_legal_hold.to_str()?)?,
            None => return Err(AzureError::MissingHeaderError(HAS_LEGAL_HOLD.to_owned())),
        };

        let mut metadata: HashMap<String, String> = HashMap::new();
        for (key, value) in headers {
            if key.as_str().starts_with(META_PREFIX) {
                metadata.insert(key.as_str().to_owned(), value.to_str()?.to_owned());
            }
        }

        Ok(Container {
            name,
            last_modified,
            e_tag,
            lease_status,
            lease_state,
            lease_duration,
            public_access,
            has_immutability_policy,
            has_legal_hold,
            metadata,
        })
    }

    fn parse(elem: &Element) -> Result<Container, AzureError> {
        let name = cast_must::<String>(elem, &["Name"])?;
        let last_modified = cast_must::<DateTime<Utc>>(elem, &["Properties", "Last-Modified"])?;
        let e_tag = cast_must::<String>(elem, &["Properties", "Etag"])?;

        let lease_state = cast_must::<LeaseState>(elem, &["Properties", "LeaseState"])?;

        let lease_duration = cast_optional::<LeaseDuration>(elem, &["Properties", "LeaseDuration"])?;

        let lease_status = cast_must::<LeaseStatus>(elem, &["Properties", "LeaseStatus"])?;

        let public_access = match cast_optional::<PublicAccess>(elem, &["Properties", "PublicAccess"])? {
            Some(pa) => pa,
            None => PublicAccess::None,
        };

        let has_immutability_policy = cast_must::<bool>(elem, &["Properties", "HasImmutabilityPolicy"])?;
        let has_legal_hold = cast_must::<bool>(elem, &["Properties", "HasLegalHold"])?;

        let metadata = {
            let mut hm = HashMap::new();
            let metadata = traverse(elem, &["Metadata"], true)?;

            for m in metadata {
                for key in &m.children {
                    let elem = match key {
                        Xml::ElementNode(elem) => elem,
                        _ => {
                            return Err(AzureError::UnexpectedXMLError(String::from(
                                "Metadata should contain an ElementNode",
                            )))
                        }
                    };

                    let key = elem.name.to_owned();

                    if elem.children.is_empty() {
                        return Err(AzureError::UnexpectedXMLError(String::from("Metadata node should not be empty")));
                    }

                    let content = {
                        match elem.children[0] {
                            Xml::CharacterNode(ref content) => content.to_owned(),
                            _ => {
                                return Err(AzureError::UnexpectedXMLError(String::from(
                                    "Metadata node should contain a CharacterNode with metadata value",
                                )))
                            }
                        }
                    };

                    hm.insert(key, content);
                }
            }

            hm
        };

        Ok(Container {
            name,
            last_modified,
            e_tag,
            lease_status,
            lease_state,
            lease_duration,
            public_access,
            has_immutability_policy,
            has_legal_hold,
            metadata,
        })
    }
}

#[inline]
pub(crate) fn generate_container_uri<'a, T>(t: &T, params: Option<&str>) -> String
where
    T: ClientRequired<'a> + ContainerNameRequired<'a>,
{
    match params {
        Some(ref params) => format!(
            "https://{}.blob.core.windows.net/{}?{}",
            t.client().account(),
            utf8_percent_encode(t.container_name(), COMPLETE_ENCODE_SET),
            params
        ),
        None => format!(
            "https://{}.blob.core.windows.net/{}",
            t.client().account(),
            utf8_percent_encode(t.container_name(), COMPLETE_ENCODE_SET),
        ),
    }
}
