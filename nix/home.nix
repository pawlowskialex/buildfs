# home-manager module: mounts and toolchain integration for this user.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  common = import ./common.nix { inherit lib self; };
  cfg = config.services.buildfs;
  variables = common.environment cfg;
  setenv = common.setenvAgent variables;
in
{
  options.services.buildfs = common.options pkgs;

  config = lib.mkIf cfg.enable {
    assertions = common.assertions cfg ++ [
      (lib.hm.assertions.assertPlatform "services.buildfs" pkgs lib.platforms.darwin)
    ];

    home.packages = [ cfg.package ];
    home.sessionVariables = variables;

    launchd.agents =
      lib.mapAttrs' (
        name: agent:
        lib.nameValuePair "buildfs-${name}" {
          enable = true;
          config = agent.serviceConfig // {
            ProgramArguments = agent.argv;
          };
        }
      ) (
        common.agents {
          inherit cfg;
          home = config.home.homeDirectory;
        }
      )
      // lib.optionalAttrs (setenv != { }) {
        buildfs-setenv = {
          enable = true;
          config = setenv;
        };
      };

    targets.darwin.defaults = common.xcodeDefaults cfg;
  };
}
