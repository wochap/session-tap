{
  description = "SessionTap local agent observability MVP";

  inputs.nixpkgs.url = "github:nixos/nixpkgs?rev=0ad6f47ea4fe188f4bc8f0380f93ae8523337c6c";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      version = (nixpkgs.lib.importTOML ./Cargo.toml).workspace.package.version;
    in {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "sessiontap";
        inherit version;
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        doCheck = false;
        nativeBuildInputs = [ pkgs.pkg-config ];
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

      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ cargo clippy rustc rustfmt pkg-config sqlite tmux cargo-deny cargo-audit ];
      };
    };
}
