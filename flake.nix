{
  description = "bas-remote: SAP BAS dev spaces with a Rust protocol layer and harness-agnostic hosts";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # The DSH bundle builder lives here; the package is only meaningful with it.
    # This is the flake the user's nix-config already pins, so `follows` in a
    # consumer works and the kernel version matches the running harness.
    deepseek-harness.url = "github:moraxyc/deepseek-harness.nix";
  };

  outputs =
    {
      self,
      nixpkgs,
      deepseek-harness,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      dshPkgs =
        system:
        (import nixpkgs { inherit system; }).extend deepseek-harness.overlays.default;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = dshPkgs system;
          # The Rust half is its own derivation so a change to the host does not
          # rebuild it, and vice versa.
          basNative = pkgs.rustPlatform.buildRustPackage {
            pname = "bas-api";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "-p"
              "bas-api"
            ];
            # The workspace also builds a CLI; only the library is packaged here.
            buildAndTestSubdir = null;
            doCheck = false;
            meta = with pkgs.lib; {
              description = "BAS protocol layer (landscape REST, dev-channel SSH, SFTP bridge)";
              homepage = "https://github.com/DataEraserC/bas-remote";
              license = licenses.asl20;
              maintainers = [ { name = "DataEraserC"; github = "DataEraserC"; } ];
              platforms = platforms.unix;
            };
          };
        in
        {
          bas-native = basNative;

          default = pkgs.dsh.buildDshBundle (finalAttrs: {
            pname = "bas-remote";
            version = (builtins.fromJSON (builtins.readFile ./hosts/dsh/package.json)).version;
            src = ./hosts/dsh;
            npmFlags = [ "--legacy-peer-deps" ];
            # Regenerate with `nix build .#default` after `npm install` changes
            # package-lock.json; the error names the hash it got.
            npmDepsHash = "sha256-zUltk4YRO/zi0t+B8Kc2pfjF6Sz9gGu2cMqjxEC4EDQ=";
            dontNpmBuild = true;
            linkKernelNodeModules = pkgs.dsh.dsh-kernel;

            postInstall = ''
              # The protocol layer ships beside the host, and the loader finds it
              # at `native/` without any environment variable.
              mkdir -p $out/lib/node_modules/bas-remote/native
              install -m 0755 ${basNative}/lib/libbas_api.so \
                $out/lib/node_modules/bas-remote/native/libbas_api.so
              test -f $out/lib/node_modules/bas-remote/lib/client.js
            '';

            meta = with pkgs.lib; {
              description = "SAP Business Application Studio remote dev spaces, as a Rust library with a DeepSeek Harness host";
              homepage = "https://github.com/DataEraserC/bas-remote";
              changelog = "https://github.com/DataEraserC/bas-remote/blob/main/CHANGELOG.md";
              license = licenses.asl20;
              maintainers = [ { name = "DataEraserC"; github = "DataEraserC"; } ];
              platforms = platforms.unix;
            };
          });
        }
      );
    };
}
