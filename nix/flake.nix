{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      ...
    }:
    let
      pkgs = import nixpkgs {
        system = "x86_64-linux";
        config.allowUnfree = true;
      };

      llvm = pkgs.llvmPackages_22;

      rust =
        with fenix.packages."x86_64-linux";
        combine [
          complete.cargo
          complete.clippy
          complete.rustc
          complete.rustfmt
          complete.rust-src
          complete.rust-analyzer
          complete.rust-std
        ];

      claude =
        let
          settings = pkgs.writeTextFile {
            name = "claude-settings";
            text = builtins.toJSON {
              "env" = {
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC" = "1";
              };
              "permissions" = {
                "allow" = [
                  "Bash(rustfmt:*)"
                ];
                "deny" = [
                  "Bash(git:*)"
                  "Bash(jj:*)"
                  "Bash(op:*)"
                ];
              };
              "sandbox" = {
                "enabled" = true;
                "failIfUnavailable" = true;
                "autoAllowBashIfSandboxed" = true;
                "allowUnsandboxedCommands" = false;
                "enableWeakerNestedSandbox" = true; # Already inside a `/proc` sandbox.
                "excludedCommands" = [ ];
                "filesystem" = {
                  "allowRead" = [ "/" ]; # Already in a filesystem sandbox.
                  "allowWrite" = [ "/" ]; # Already in a filesystem sandbox.
                  "allowManagedPermissionRulesOnly" = true;
                  "allowManagedReadPathsOnly" = true;
                };
                "network" = {
                  "allowedDomains" = [ ];
                  "allowUnixSockets" = [ ];
                  "allowLocalBinding" = false;
                  "allowAllUnixSockets" = false;
                  "allowManagedDomainsOnly" = true;
                };
              };
            };
          };
        in
        pkgs.writeTextFile {
          name = "claude-wrapper";
          executable = true;
          destination = "/bin/claude";
          text = ''
            #!${pkgs.runtimeShell}

            mkdir --parents "$HOME/.npm-global"
            [ -s "$HOME/.claude.json" ] || echo '{}' > "$HOME/.claude.json"

            exec -a claude ${pkgs.bubblewrap}/bin/bwrap \
                --proc /proc \
                --dev /dev \
                --tmpfs /run \
                --tmpfs /tmp \
                --ro-bind /bin/sh /bin/sh \
                --ro-bind "${settings}" /etc/claude-code/managed-settings.json \
                --ro-bind /etc/resolv.conf /etc/resolv.conf \
                --ro-bind /etc/ssl /etc/ssl \
                --ro-bind /etc/static/ssl /etc/static/ssl \
                --ro-bind /nix/store /nix/store \
                --ro-bind /run/current-system/sw/bin /run/current-system/sw/bin \
                --ro-bind /run/wrappers/bin /run/wrappers/bin \
                --ro-bind /sys /sys \
                --bind "$HOME/.npm-global" "$HOME/.npm-global" \
                --bind "$HOME/.cache" "$HOME/.cache" \
                --bind "$HOME/.claude" "$HOME/.claude" \
                --bind "$HOME/.claude.json" "$HOME/.claude.json" \
                --bind "$HOME/jj" "$HOME/jj" \
                --setenv CACHE_DIR "$CACHE_DIR" \
                --setenv HOME "$HOME" \
                --setenv PATH "$PATH" \
                --setenv SHELL "$SHELL" \
                --setenv TERM "$TERM" \
                --setenv WORKSPACE_DIR "$WORKSPACE_DIR" \
                --die-with-parent \
                -- ${pkgs.claude-code}/bin/claude "$@"
          '';
        };
    in
    {
      devShells."x86_64-linux".default = pkgs.mkShellNoCC {
        packages = [
          llvm.clang
          rust
          claude
        ];
      };
    };
}
