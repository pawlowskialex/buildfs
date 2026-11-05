# nix-darwin module: mounts and toolchain integration for the primary user.
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
  xcode = common.xcodeDefaults cfg;
in
{
  options.services.buildfs = common.options pkgs // {
    user = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = config.system.primaryUser;
      defaultText = lib.literalExpression "config.system.primaryUser";
      description = "The user whose home holds the stores and logs.";
    };

    home = lib.mkOption {
      type = lib.types.str;
      default =
        let
          declared = config.users.users.${cfg.user}.home or null;
        in
        if declared != null then declared else "/Users/${cfg.user}";
      defaultText = lib.literalExpression ''"/Users/''${config.services.buildfs.user}"'';
      description = "That user's home directory.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = common.assertions cfg ++ [
      {
        assertion = cfg.user != null;
        message = "services.buildfs.user is unset; set it or system.primaryUser.";
      }
    ];

    environment.systemPackages = [ cfg.package ];
    environment.variables = variables;

    launchd.user.agents =
      lib.mapAttrs' (
        name: agent:
        lib.nameValuePair "buildfs-${name}" {
          command = lib.escapeShellArgs agent.argv;
          inherit (agent) serviceConfig;
        }
      ) (common.agents { inherit cfg; inherit (cfg) home; })
      // lib.optionalAttrs (setenv != { }) { buildfs-setenv.serviceConfig = setenv; };

    system.defaults.CustomUserPreferences = xcode;
  };
}
