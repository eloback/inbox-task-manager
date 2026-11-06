# home-manager module: installs itm and runs its daemon as a systemd user
# service. Herdr's key bindings stay in your own Herdr config; README.md
# lists them.
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.inbox-task-manager;
  exe = lib.getExe cfg.package;

  # Service managers start the daemon without your shell's environment: find
  # your herdr first (the client must match the server), and the same tasks
  # and Herdr sessions as a login shell.
  daemonEnvironment = {
    PATH = lib.concatStringsSep ":" [
      "${config.home.profileDirectory}/bin"
      "/etc/profiles/per-user/${config.home.username}/bin"
      "/run/current-system/sw/bin"
      "/usr/local/bin"
      "/usr/bin"
      "/bin"
    ];
    XDG_CONFIG_HOME = config.xdg.configHome;
    XDG_STATE_HOME = config.xdg.stateHome;
    XDG_DATA_HOME = config.xdg.dataHome;
  };
in
{
  options.programs.inbox-task-manager = {
    enable = lib.mkEnableOption "inbox-task-manager (itm), an inbox for Herdr task sessions";

    package = lib.mkOption {
      type = lib.types.package;
      default =
        self.packages.${pkgs.stdenv.hostPlatform.system}.default or (pkgs.callPackage ./package.nix { });
      defaultText = lib.literalExpression "inbox-task-manager.packages.\${system}.default";
      description = "The inbox-task-manager package to install.";
    };

    daemon.enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Run `itm daemon` as a user service: it notifies about agents waiting for
        input and wakes snoozed tasks.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      { home.packages = [ cfg.package ]; }

      (lib.mkIf cfg.daemon.enable {
        systemd.user.services.inbox-task-manager = {
          Unit = {
            Description = "inbox-task-manager agent notifications and snooze timers";
            # Desktop notifications need the graphical session's notification daemon.
            After = [ "graphical-session.target" ];
          };
          Service = {
            ExecStart = "${exe} daemon";
            Environment = lib.mapAttrsToList (name: value: "${name}=${value}") daemonEnvironment;
            Restart = "on-failure";
            RestartSec = 5;
          };
          Install.WantedBy = [ "default.target" ];
        };
      })
    ]
  );
}
