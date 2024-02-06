pub mod google {
    pub mod api {
        include!("google.api.rs");
    }
    pub mod bytestream {
        include!("google.bytestream.rs");
    }
    pub mod longrunning {
        include!("google.longrunning.rs");
    }
    pub mod rpc {
        include!("google.rpc.rs");
    }
}
pub mod build {
    pub mod bazel {
        pub mod semver {
            include!("build.bazel.semver.rs");
        }
        pub mod remote {
            pub mod execution {
                pub mod v2 {
                    include!("build.bazel.remote.execution.v2.rs");
                }
            }
        }
    }
}
