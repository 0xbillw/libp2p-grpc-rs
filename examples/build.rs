fn main() {
	tonic_build::compile_protos("proto/nodeinfo/nodeinfo.proto").unwrap();
}