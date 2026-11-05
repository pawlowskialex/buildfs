# What the nix-darwin and home-manager modules share.
{ lib, self }:

let
  inherit (lib) mkOption types;

  mountModule = {
    options = {
      mountPoint = mkOption {
        type = types.strMatching "/.*";
        example = "/Users/alex/mnt/build";
        description = "Where to mount the store: an absolute path, since launchd doesn't expand `~`.";
      };

      backend = mkOption {
        type = types.enum [
          "fskit"
          "nfs"
        ];
        description = ''
          How to serve the store. There's no default because the choice has to stay
          fixed once the agent is installed, and whether the FSKit module is present
          can't be known when the configuration is built.

          `fskit` is much faster, but needs macOS 27 and the signed module, which this
          flake can't build: run `fskit/install.sh` and enable the extension in System
          Settings. `nfs` works without any setup. Stores can move between the two,
          but attachments can't.
        '';
      };

      port = mkOption {
        type = types.port;
        default = 20490;
        description = "NFS server port. Each NFS mount needs its own.";
      };

      sync = mkOption {
        type = types.enum [
          "relaxed"
          "strict"
        ];
        default = "relaxed";
        description = ''
          `relaxed` survives buildfs crashes. `strict` also aims to survive power loss,
          which hasn't been tested, and makes a clean build about 2.5x slower on FSKit.
        '';
      };

      level = mkOption {
        type = types.ints.between 1 19;
        default = 3;
        description = "zstd compression level.";
      };

      compressAfter = mkOption {
        type = types.ints.unsigned;
        default = 15;
        description = "Compress files this many seconds after their last write (0 = never).";
      };

      workers = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "Background compression threads (default: a quarter of the cores).";
      };
    };
  };

  # Environment variables take precedence over each tool's config file, so these
  # can't conflict with a config file managed some other way, or with `go env -w`.
  envFor = {
    rust.CARGO_BUILD_BUILD_DIR = "cargo/{workspace-path-hash}";
    go.GOCACHE = "go/cache";
    uv.UV_CACHE_DIR = "uv/cache";
    zig.ZIG_GLOBAL_CACHE_DIR = "zig/cache";
    gradle.GRADLE_USER_HOME = "gradle";
    python.PYTHONPYCACHEPREFIX = "pycache";
  };

  # What `integrate` points at.
  integrationRoot =
    cfg:
    let
      names = lib.attrNames cfg.mounts;
      name =
        if cfg.integrate.mount != null then
          cfg.integrate.mount
        else if lib.length names == 1 then
          lib.head names
        else
          null;
    in
    if name != null && cfg.mounts ? ${name} then cfg.mounts.${name}.mountPoint else null;

  tuningArgs =
    mount:
    [
      "--level"
      (toString mount.level)
      "--compress-after"
      (toString mount.compressAfter)
      "--sync"
      mount.sync
    ]
    ++ lib.optionals (mount.workers != null) [
      "--workers"
      (toString mount.workers)
    ];
in
{
  options = pkgs: {
    enable = lib.mkEnableOption "buildfs mounts";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "buildfs.packages.\${system}.default";
      description = "The buildfs package.";
    };

    mounts = mkOption {
      type = types.attrsOf (types.submodule mountModule);
      default = { };
      example = lib.literalExpression ''
        {
          default = {
            mountPoint = "/Users/alex/mnt/build";
            backend = "fskit";
          };
        }
      '';
      description = ''
        Mounts to keep running, by store name. The store is
        `~/.buildfs/stores/<name>`.

        Before removing an NFS mount from this list, unmount it: removing it stops its
        server, and the mount left behind hangs anything that touches it until
        `umount -f`.
      '';
    };

    integrate = {
      tools = mkOption {
        type = types.listOf (
          types.enum [
            "rust"
            "xcode"
            "go"
            "uv"
            "zig"
            "gradle"
            "python"
          ]
        );
        default = [ ];
        example = [
          "rust"
          "xcode"
          "go"
          "uv"
        ];
        description = ''
          Toolchains whose artifacts and caches go on the mount, the declarative
          version of `buildfs integrate`. Xcode is set through its defaults, and the
          others through environment variables: for shells, and with `launchctl
          setenv` for GUI apps, which need restarting to see them. No config file is
          touched, and these override what's in one.

          Removing a tool here takes effect in new shells, but GUI apps keep the
          variable until you log out, or run `launchctl unsetenv`.
        '';
      };

      mount = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Which of `mounts` to use. Can be left unset if there's only one.";
      };
    };
  };

  # launchd configuration of each mount's agent, by store name: the same behaviour
  # as `buildfs install-agent`.
  agents =
    { cfg, home }:
    lib.mapAttrs (
      name: mount:
      let
        store = "${home}/.buildfs/stores/${name}";
        log = "${home}/Library/Logs/buildfs-${name}.log";
        arguments =
          (
            if mount.backend == "fskit" then
              [
                "--fskit"
                "--name"
                name
              ]
            else
              [
                # launchd jobs can't reliably unmount, so the mount stays up, and a
                # restarted server adopts it.
                "--keep-mounted"
                "--store"
                store
                "--port"
                (toString mount.port)
              ]
          )
          ++ tuningArgs mount
          ++ [ mount.mountPoint ];
      in
      {
        argv = [
          (lib.getExe cfg.package)
          "mount"
        ]
        ++ arguments;
        serviceConfig = {
          Label = "dev.buildfs.${name}";
          RunAtLoad = true;
          StandardOutPath = log;
          StandardErrorPath = log;
          # `buildfs` reads these, for example to refuse unmounting a volume that an
          # agent serves. launchd's wrappers make the arguments unreadable.
          EnvironmentVariables = {
            BUILDFS_MANAGED_MOUNTPOINT = mount.mountPoint;
            BUILDFS_MANAGED_STORE = store;
            BUILDFS_MANAGED_BACKEND = mount.backend;
          };
        }
        // (
          if mount.backend == "fskit" then
            {
              # There's no server to keep alive: mounting does nothing if all is
              # mounted, and recovers if the extension died.
              StartInterval = 30;
              ProcessType = "Background";
            }
          else
            {
              KeepAlive.SuccessfulExit = false;
              ThrottleInterval = 5;
              ProcessType = "Interactive";
            }
        );
      }
    ) cfg.mounts;

  # The variables that `integrate.tools` asks for.
  environment =
    cfg:
    let
      root = integrationRoot cfg;
      tools = lib.filter (tool: envFor ? ${tool}) cfg.integrate.tools;
    in
    lib.optionalAttrs (root != null) (
      lib.foldl' (
        vars: tool: vars // lib.mapAttrs (_: relative: "${root}/${relative}") envFor.${tool}
      ) { } tools
      # uv falls back to copying across file systems, with a warning.
      // lib.optionalAttrs (lib.elem "uv" tools) { UV_LINK_MODE = "copy"; }
    );

  # An agent that gives GUI apps the same variables, if there are any.
  setenvAgent =
    variables:
    lib.optionalAttrs (variables != { }) {
      Label = "dev.buildfs-setenv";
      RunAtLoad = true;
      ProgramArguments = [
        "/bin/launchctl"
        "setenv"
      ]
      ++ lib.concatLists (lib.mapAttrsToList (name: value: [ name value ]) variables);
    };

  # Xcode's settings, if it's among the tools.
  xcodeDefaults =
    cfg:
    let
      root = integrationRoot cfg;
    in
    lib.optionalAttrs (root != null && lib.elem "xcode" cfg.integrate.tools) {
      "com.apple.dt.Xcode".IDECustomDerivedDataLocation = "${root}/xcode/DerivedData";
    };

  assertions =
    cfg:
    let
      nfsPorts = lib.mapAttrsToList (_: mount: mount.port) (
        lib.filterAttrs (_: mount: mount.backend == "nfs") cfg.mounts
      );
    in
    [
      {
        assertion = lib.allUnique nfsPorts;
        message = "Each NFS mount in services.buildfs.mounts needs its own port.";
      }
      {
        assertion = cfg.integrate.tools == [ ] || integrationRoot cfg != null;
        message = ''
          services.buildfs.integrate.mount has to name one of services.buildfs.mounts, unless
          there's exactly one.
        '';
      }
    ]
    ++ lib.mapAttrsToList (name: _: {
      assertion = builtins.match "[a-z0-9][a-z0-9-]{0,63}" name != null && !lib.hasPrefix "att-" name;
      message = ''
        services.buildfs.mounts."${name}": store names may only contain a-z, 0-9 and '-'
        (64 at most), can't start with '-', and can't start with "att-", which is
        reserved for attachments.
      '';
    }) cfg.mounts;
}
