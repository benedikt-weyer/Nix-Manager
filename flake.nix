{
  description = "Nix-Manager development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          inherit (pkgs) lib;
          craneLib = crane.mkLib pkgs;

          # The workspace builds from the repo root. Besides Rust sources it
          # needs the proto contract (which lives in the backend's directory,
          # but only the .proto - the backend itself is built separately) and
          # the assets the GUI embeds and installs.
          workspaceSrc = lib.cleanSourceWith {
            src = ./.;
            name = "nix-manager-source";
            filter =
              path: type:
              let
                rel = lib.removePrefix "${toString ./.}/" (toString path);
                inBackend = lib.hasPrefix "nix-backend-generic" rel;
              in
              if inBackend then
                type == "directory" || lib.hasSuffix ".proto" rel
              else
                craneLib.filterCargoSources path type || lib.hasPrefix "assets" rel;
          };

          # The backend is a standalone crate with its own lockfile.
          backendSrc = lib.cleanSourceWith {
            src = ./nix-backend-generic;
            name = "nix-backend-generic-source";
            filter = path: type: craneLib.filterCargoSources path type || lib.hasSuffix ".proto" path;
          };

          # Frontends and the backend share one socket by default, distinct
          # from the one the shell's own daemon uses, so installing this never
          # fights over (or deletes) that socket.
          defaultSocket = ''
            --run 'export NIX_BACKEND_SOCKET="''${NIX_BACKEND_SOCKET:-''${XDG_RUNTIME_DIR:-/tmp}/nix-manager.sock}"' '';

          guiLibraries = lib.makeLibraryPath (
            with pkgs;
            [
              fontconfig
              libGL
              libx11
              libxcursor
              libxi
              libxkbcommon
              libxrandr
              vulkan-loader
              wayland
            ]
          );

          workspaceArgs = {
            pname = "nix-manager";
            version = "0.1.0";
            src = workspaceSrc;
            strictDeps = true;
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.protobuf
            ];
            buildInputs = [ pkgs.fontconfig ];
          };
          workspaceDeps = craneLib.buildDepsOnly workspaceArgs;

          backendArgs = {
            src = backendSrc;
            strictDeps = true;
            nativeBuildInputs = [ pkgs.protobuf ];
            # The git module's tests run real git commands.
            nativeCheckInputs = [ pkgs.git ];
          };

          nix-backend-generic = craneLib.buildPackage (
            backendArgs
            // {
              cargoArtifacts = craneLib.buildDepsOnly backendArgs;
              nativeBuildInputs = backendArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];
              postInstall = ''
                wrapProgram $out/bin/nix-backend-generic \
                  ${defaultSocket} \
                  --suffix PATH : ${lib.makeBinPath [ pkgs.nix pkgs.git ]}
              '';
              meta.mainProgram = "nix-backend-generic";
            }
          );

          nix-manager-cli = craneLib.buildPackage (
            workspaceArgs
            // {
              pname = "nix-manager-cli";
              cargoArtifacts = workspaceDeps;
              cargoExtraArgs = "-p nix-frontend-cli";
              nativeBuildInputs = workspaceArgs.nativeBuildInputs ++ [
                pkgs.installShellFiles
                pkgs.makeWrapper
              ];
              # Completions come from the binary itself, so generate them
              # before it gets wrapped.
              postInstall = ''
                installShellCompletion --cmd nix-manager \
                  --bash <($out/bin/nix-manager completions bash) \
                  --zsh <($out/bin/nix-manager completions zsh) \
                  --fish <($out/bin/nix-manager completions fish)
                wrapProgram $out/bin/nix-manager ${defaultSocket}
              '';
              meta.mainProgram = "nix-manager";
            }
          );

          nix-manager-gui = craneLib.buildPackage (
            workspaceArgs
            // {
              pname = "nix-manager-gui";
              cargoArtifacts = workspaceDeps;
              cargoExtraArgs = "-p nix-frontend-iced";
              nativeBuildInputs = workspaceArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];
              # iced loads its windowing and graphics libraries at runtime.
              postInstall = ''
                install -Dm644 assets/icons/nix-manager.svg \
                  $out/share/icons/hicolor/scalable/apps/nix-manager.svg
                install -Dm644 assets/nix-manager.desktop $out/share/applications/nix-manager.desktop

                wrapProgram $out/bin/nix-frontend-iced \
                  ${defaultSocket} \
                  --suffix LD_LIBRARY_PATH : ${guiLibraries}:/run/opengl-driver/lib
                ln -s nix-frontend-iced $out/bin/nix-manager-gui
              '';
              meta.mainProgram = "nix-manager-gui";
            }
          );
        in
        {
          inherit nix-backend-generic nix-manager-cli nix-manager-gui;

          default = pkgs.symlinkJoin {
            name = "nix-manager";
            paths = [
              nix-backend-generic
              nix-manager-cli
              nix-manager-gui
            ];
            meta.mainProgram = "nix-manager-gui";
          };
        }
      );

      checks = forAllSystems (
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          pkgsFor = self.packages.${pkgs.stdenv.hostPlatform.system};
        in
        {
          inherit (pkgsFor) nix-backend-generic nix-manager-cli nix-manager-gui;

          clippy = craneLib.cargoClippy {
            inherit (pkgsFor.nix-manager-gui) src;
            pname = "nix-manager";
            version = "0.1.0";
            strictDeps = true;
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.protobuf
            ];
            buildInputs = [ pkgs.fontconfig ];
            cargoArtifacts = craneLib.buildDepsOnly {
              inherit (pkgsFor.nix-manager-gui) src;
              pname = "nix-manager";
              version = "0.1.0";
              strictDeps = true;
              nativeBuildInputs = [
                pkgs.pkg-config
                pkgs.protobuf
              ];
              buildInputs = [ pkgs.fontconfig ];
            };
            cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
          };
        }
      );

      # A user service for the backend plus the programs, in the style of the
      # other flakes consumed by the NixOS config:
      #   inputs.nix-manager.nixosModules.default  +  programs.nix-manager.enable = true;
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.nix-manager;
        in
        {
          options.programs.nix-manager = {
            enable = lib.mkEnableOption "Nix Manager, a GUI and CLI for updating and rebuilding the system flake";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "nix-manager.packages.\${system}.default";
              description = "The Nix Manager package (backend, CLI and GUI).";
            };

            backend.enable = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = ''
                Run the backend as a systemd user service, on its own socket
                ($XDG_RUNTIME_DIR/nix-manager.sock). The GUI and CLI use that
                socket by default.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];

            systemd.user.services.nix-manager-backend = lib.mkIf cfg.backend.enable {
              description = "Nix Manager backend";
              wantedBy = [ "default.target" ];
              path = [
                config.nix.package
                pkgs.git
              ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/nix-backend-generic";
                Restart = "on-failure";
                RestartSec = 2;
              };
            };
          };
        };

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
