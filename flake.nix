{
  description = "nova — gpui+loro coding-agent controller";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # honor rust-toolchain.toml (stable + rustfmt + clippy)
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # gpui native deps (linux only — darwin links system frameworks)
        gpuiLinuxLibs = with pkgs; [
          wayland
          libxkbcommon
          vulkan-loader
          xorg.libX11
          xorg.libXcursor
          xorg.libXi
          xorg.libXrandr
          fontconfig
          freetype
          alsa-lib
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            rust-analyzer

            # rusqlite bundled (cc) + font-kit — needed on darwin too
            pkg-config
            cmake

            # scripts/ (tui-smoke, frame_png → PIL)
            (python3.withPackages (ps: [ ps.pillow ]))
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux (with pkgs; [
            # linux-only so it doesn't shadow the darwin cc wrapper
            clang
            # bindgen
            llvmPackages.libclang
          ])
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            # nix ld on darwin can't find system libiconv
            pkgs.libiconv
          ];

          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux gpuiLinuxLibs;

          shellHook = ''
            # bindgen needs libclang on every platform
            export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
          '' + pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            # gpui dlopens wayland/vulkan at runtime on NixOS
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath gpuiLinuxLibs}:$LD_LIBRARY_PATH"
          '' + pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
            export LIBRARY_PATH="${pkgs.libiconv}/lib:$LIBRARY_PATH"
          '';
        };
      });
}
