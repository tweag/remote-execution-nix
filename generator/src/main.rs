fn main() {
    let mut config = prost_build::Config::new();
    config.out_dir("out");
    std::fs::create_dir_all("out").unwrap();

    tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .out_dir("out")
        .compile_with_config(
            config,
            &[
                "build/bazel/remote/execution/v2/remote_execution.proto",
                "google/bytestream/bytestream.proto",
            ],
            &["proto/remote-apis-2.2.0", "proto/googleapis-master"],
        )
        .unwrap();
}
