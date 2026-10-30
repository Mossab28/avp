//! Where a server may listen: beyond this machine only once AVP_TOKEN checks who asks, in every
//! default build. A test binary of its own, since it sets the variable other tests' servers read.
#![cfg(feature = "cloud")]

#[test]
fn a_server_listens_beyond_this_machine_only_with_a_token() {
    std::env::remove_var("AVP_TOKEN");
    let err = avp::seam::may_listen("0.0.0.0:7117").unwrap_err();
    assert!(format!("{err:#}").contains("set AVP_TOKEN"), "{err:#}");
    std::env::set_var("AVP_TOKEN", "a-team-token");
    avp::seam::may_listen("0.0.0.0:7117").unwrap();
}
