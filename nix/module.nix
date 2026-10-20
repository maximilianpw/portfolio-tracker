self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.portfolio-tracker;
  dataDir = "/var/lib/portfolio-tracker";

  hardening = {
    User = "portfolio-tracker";
    Group = "portfolio-tracker";
    StateDirectory = "portfolio-tracker";
    StateDirectoryMode = "0700";
    WorkingDirectory = dataDir;
    UMask = "0077";
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    PrivateDevices = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    ProtectControlGroups = true;
    ProtectClock = true;
    ProtectHostname = true;
    RestrictAddressFamilies = [
      "AF_INET"
      "AF_INET6"
      "AF_UNIX"
    ];
    RestrictNamespaces = true;
    RestrictRealtime = true;
    RestrictSUIDSGID = true;
    LockPersonality = true;
    CapabilityBoundingSet = "";
    SystemCallArchitectures = "native";
  };

  environment = {
    PT_DATA_DIR = dataDir;
    HOME = dataDir;
  }
  // lib.optionalAttrs (cfg.correctionsFile != null) { PT_CORRECTIONS = "${cfg.correctionsFile}"; };
in
{
  options.services.portfolio-tracker = {
    enable = lib.mkEnableOption "the read-only Trade Republic portfolio tracker";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "portfolio-tracker.packages.\${system}.default";
    };

    pytrPackage = lib.mkPackageOption pkgs "pytr" { };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8787";
      description = "Address the read-only API listens on. Put it behind the reverse proxy; keep it off the public internet.";
    };

    tokenFile = lib.mkOption {
      type = lib.types.path;
      description = "File containing the API bearer token (at least 16 characters), e.g. a sops-nix secret.";
    };

    credentialsFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        pytr credentials file (phone number and PIN on two lines), e.g. a sops-nix secret. Copied to
        ~/.pytr/credentials before each sync. When null, log in once by hand with
        `pytr login --store_credentials --v2` as the service user.
      '';
    };

    instrumentsFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "instruments.toml mapping ISINs to TradingView symbols. Without it, no TradingView CSVs are written.";
    };

    correctionsFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "corrections.toml with reviewed opening balances and adjustments for what the timeline doesn't contain.";
    };

    syncAt = lib.mkOption {
      type = lib.types.str;
      default = "*-*-* 23:15:00 Europe/Berlin";
      description = "systemd OnCalendar expression for the daily sync. Trade Republic's venue closes at 23:00 Berlin.";
    };

    onFailure = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "notify-failure@%n.service" ];
      description = "Units to start when the sync fails, e.g. to send an alert.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.portfolio-tracker = {
      isSystemUser = true;
      group = "portfolio-tracker";
      home = dataDir;
    };
    users.groups.portfolio-tracker = { };

    environment.systemPackages = [
      (pkgs.writeShellScriptBin "portfolio-tracker-login" ''
        install -d -m 0700 -o portfolio-tracker -g portfolio-tracker ${dataDir}
        exec ${pkgs.util-linux}/bin/runuser -u portfolio-tracker -- \
          env HOME=${dataDir} ${lib.getExe cfg.pytrPackage} login --store_credentials --v2 "$@"
      '')
    ];

    systemd.services.portfolio-tracker-api = {
      description = "Portfolio tracker read-only API";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      environment = environment // {
        PT_LISTEN = cfg.listen;
        PT_TOKEN_FILE = "%d/token";
      };
      serviceConfig = hardening // {
        ExecStart = "${lib.getExe cfg.package} serve";
        LoadCredential = [ "token:${cfg.tokenFile}" ];
        Restart = "on-failure";
        MemoryDenyWriteExecute = true;
      };
    };

    systemd.services.portfolio-tracker-sync = {
      description = "Portfolio tracker daily sync";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      onFailure = cfg.onFailure;
      environment =
        environment
        // {
          PT_PYTR = lib.getExe cfg.pytrPackage;
        }
        // lib.optionalAttrs (cfg.instrumentsFile != null) { PT_INSTRUMENTS = "${cfg.instrumentsFile}"; };
      serviceConfig =
        hardening
        // {
          Type = "oneshot";
          ExecStart = "${lib.getExe cfg.package} sync";
        }
        // lib.optionalAttrs (cfg.credentialsFile != null) {
          LoadCredential = [ "pytr:${cfg.credentialsFile}" ];
          ExecStartPre = pkgs.writeShellScript "portfolio-tracker-credentials" ''
            install -d -m 0700 ${dataDir}/.pytr
            install -m 0600 "$CREDENTIALS_DIRECTORY/pytr" ${dataDir}/.pytr/credentials
          '';
        };
    };

    systemd.timers.portfolio-tracker-sync = {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.syncAt;
        Persistent = true;
        RandomizedDelaySec = "5m";
      };
    };
  };
}
