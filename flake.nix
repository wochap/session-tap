{
  description = "SessionTap local agent observability MVP";

  inputs.nixpkgs.url = "github:nixos/nixpkgs?rev=0ad6f47ea4fe188f4bc8f0380f93ae8523337c6c";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      version = (nixpkgs.lib.importTOML ./Cargo.toml).workspace.package.version;

      androidPkgs = import nixpkgs {
        inherit system;
        config = { allowUnfree = true; android_sdk.accept_license = true; };
      };
      androidBuildTools = "36.0.0";
      androidComposition = emulator: androidPkgs.androidenv.composeAndroidPackages ({
        platformVersions = [ "36" ];
        buildToolsVersions = [ androidBuildTools ];
        includeEmulator = emulator;
        includeSystemImages = emulator;
        includeSources = false;
        includeNDK = false;
      } // nixpkgs.lib.optionalAttrs emulator {
        systemImageTypes = [ "google_apis" ];
        abiVersions = [ "x86_64" ];
      });
      androidShell = emulator:
        let
          sdk = (androidComposition emulator).androidsdk;
          sdkRoot = "${sdk}/libexec/android-sdk";
        in androidPkgs.mkShell {
          packages = [ sdk androidPkgs.jdk21 androidPkgs.android-tools ]
            # test-hub.sh drives an isolated hub through its socket and ingestion port.
            ++ nixpkgs.lib.optionals emulator (with androidPkgs; [ curl jq socat cargo rustc pkg-config openssl sqlite ]);
          ANDROID_HOME = sdkRoot;
          ANDROID_SDK_ROOT = sdkRoot;
          JAVA_HOME = androidPkgs.jdk21.home;
          GRADLE_OPTS = "-Dorg.gradle.project.android.aapt2FromMavenOverride=${sdkRoot}/build-tools/${androidBuildTools}/aapt2";
        };
    in {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "sessiontap";
        inherit version;
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        doCheck = false;
        nativeBuildInputs = [ pkgs.pkg-config ];
        nativeCheckInputs = [ pkgs.util-linux ];
        buildInputs = [ pkgs.openssl pkgs.sqlite ];
        postInstall = ''
          install -d $out/share/zsh/site-functions
          install -m 644 completions/zsh/_sessiontap completions/zsh/_sessiontapd completions/zsh/_sessiontap-hub $out/share/zsh/site-functions/
        '';
        meta = {
          description = "Local observability for explicitly wrapped coding agents";
          longDescription = ''
            SessionTap packages the sessiontap client and sessiontapd daemon.
            Consumers must start sessiontapd explicitly; the client does not
            spawn or supervise it.
          '';
          license = nixpkgs.lib.licenses.mit;
          mainProgram = "sessiontap";
          platforms = [ system ];
        };
      };

      apps.${system}.default = {
        type = "app";
        program = "${self.packages.${system}.default}/bin/sessiontap";
      };

      devShells.${system} = {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo clippy rustc rustfmt pkg-config sqlite tmux cargo-deny cargo-audit ];
        };
        android = androidShell false;
        android-emulator = androidShell true;
      };
    };
}
