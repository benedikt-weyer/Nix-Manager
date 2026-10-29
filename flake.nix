{
  description = "Nix-Manager development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          name = "nix-manager";

          nativeBuildInputs = with pkgs; [
            pkg-config
            protobuf # protoc, needed by tonic-build in the backend and the client crate
          ];

          packages = with pkgs; [
            # Rust toolchain
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer

            # Dev tooling used by scripts/
            cargo-watch
            grpcurl
            iproute2 # ss, used by the run scripts to detect a live backend
            jq
          ];

          # Libraries the GUI frontends load at runtime (iced -> winit/wgpu).
          # They are dlopen()ed rather than linked, so they have to be on
          # LD_LIBRARY_PATH.
          buildInputs = with pkgs; [
            fontconfig
            libxkbcommon
            libGL
            vulkan-loader
            wayland
            libx11
            libxcursor
            libxi
            libxrandr
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          shellHook = ''
            export LD_LIBRARY_PATH="${
              pkgs.lib.makeLibraryPath (
                with pkgs;
                [
                  libxkbcommon
                  libGL
                  vulkan-loader
                  wayland
                  libx11
                  libxcursor
                  libxi
                  libxrandr
                ]
              )
            }:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

            export NIX_MANAGER_ROOT="$PWD"

            echo "nix-manager dev shell - run ./scripts/help for the available commands"
          '';
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
