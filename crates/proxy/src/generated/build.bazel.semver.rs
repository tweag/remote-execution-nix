/// The full version of a given tool.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SemVer {
    /// The major version, e.g 10 for 10.2.3.
    #[prost(int32, tag = "1")]
    pub major: i32,
    /// The minor version, e.g. 2 for 10.2.3.
    #[prost(int32, tag = "2")]
    pub minor: i32,
    /// The patch version, e.g 3 for 10.2.3.
    #[prost(int32, tag = "3")]
    pub patch: i32,
    /// The pre-release version. Either this field or major/minor/patch fields
    /// must be filled. They are mutually exclusive. Pre-release versions are
    /// assumed to be earlier than any released versions.
    #[prost(string, tag = "4")]
    pub prerelease: ::prost::alloc::string::String,
}
