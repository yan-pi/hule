{
  description = "Hule development-artifact retention CLI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.nixpkgs-x86_64-darwin.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-x86_64-darwin,
    }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system:
        import
          (if system == "x86_64-darwin" then nixpkgs-x86_64-darwin else nixpkgs)
          { inherit system; };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          hule = pkgs.rustPlatform.buildRustPackage {
            pname = "hule";
            version = "0.1.0";
            src = nixpkgs.lib.cleanSourceWith {
              src = ./.;
              filter = path: _type:
                let
                  relativePath = nixpkgs.lib.removePrefix "${toString ./.}/" (toString path);
                in
                relativePath != ".git"
                && relativePath != "target"
                && !(nixpkgs.lib.hasPrefix "target/" relativePath);
            };
            cargoLock.lockFile = ./Cargo.lock;
          };
        in
        {
          default = hule;
          inherit hule;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc clippy rustfmt ];
          };
        }
      );
    };
}
