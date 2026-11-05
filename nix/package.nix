# The CLI and the NFS backend. The FSKit module has to be signed with an Apple
# developer team, so it can't be built here; see fskit/install.sh.
{ lib, rustPlatform }:

let
  root = ../.;
in
rustPlatform.buildRustPackage {
  pname = "buildfs";
  version = (lib.importTOML ../crates/buildfs/Cargo.toml).package.version;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../vendor
    ];
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    # Only the vendored embednfs's tests use this, but Cargo.lock lists it.
    outputHashes."nfs-rs-0.2.0" = "sha256-bSzAIJTYl6V8ZoOWFiIsdPXsC3mjd3y2Dz7pUZXglZw=";
  };
  cargoBuildFlags = [ "-p" "buildfs" ];
  cargoTestFlags = [ "-p" "buildfs" ];

  # The tests bind ports on localhost.
  __darwinAllowLocalNetworking = true;

  meta = {
    description = "Compressing, deduplicating filesystem for build artifacts on macOS";
    homepage = "https://github.com/pawlowskialex/buildfs";
    license = lib.licenses.mit;
    mainProgram = "buildfs";
    platforms = lib.platforms.darwin;
  };
}
