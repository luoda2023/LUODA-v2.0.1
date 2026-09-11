import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:luoda_flutter/mobile/pages/server_page.dart';
import 'package:luoda_flutter/mobile/pages/settings_page.dart';
import 'package:luoda_flutter/runtime_logger.dart';
import 'package:luoda_flutter/web/settings_page.dart';
import 'package:get/get.dart';
import '../../common.dart';
import '../../common/widgets/chat_page.dart';
import '../../consts.dart';
import '../../models/platform_model.dart';
import '../../models/state_model.dart';
import '../first_run_permission_flow.dart';
import 'connection_page.dart';
import 'scan_page.dart';

const _kFirstRunAuthorization = 'android-first-run-authorization-v2';
const _kPublicAuthMarkerName = 'first-run-authorization';

/// Persisted authorization marker on the *real public* external storage
/// (/storage/emulated/0/Android/data/... is app-private and wiped together
/// with the app data on uninstall, so we deliberately use a folder that
/// survives reinstall). path_provider's external dirs are app-scoped, so the
/// path is resolved natively from Kotlin and cached in the Rust LocalConfig
/// (history-backup-dir). Falling back to the app-private marker keeps
/// "reinstalled over the same version" working even when the storage
/// permission was revoked (Android 13+ / no MANAGE_EXTERNAL_STORAGE grant).
String _authorizationBaseDir() {
  final dir = bind.mainGetLocalOption(key: kOptionAuthorizationBaseDir);
  if (dir.isNotEmpty) return dir;
  final backupDir = bind.mainGetLocalOption(key: kOptionHistoryBackupDir);
  if (backupDir.isNotEmpty) return File(backupDir).parent.path;
  return '';
}

File? _publicAuthMarkerFile() {
  final dir = _authorizationBaseDir();
  if (dir.isEmpty) return null;
  return File('$dir${Platform.pathSeparator}$_kPublicAuthMarkerName');
}

bool _readPublicAuthMarker() {
  try {
    final f = _publicAuthMarkerFile();
    if (f == null) return false;
    return f.existsSync() && f.readAsStringSync().trim() == 'Y';
  } catch (_) {
    return false;
  }
}

void _writePublicAuthMarker() {
  try {
    final f = _publicAuthMarkerFile();
    if (f == null) return;
    f.parent.createSync(recursive: true);
    f.writeAsStringSync('Y');
  } catch (_) {}
}

abstract class PageShape extends Widget {
  final String title = "";
  final Widget icon = Icon(null);
  final List<Widget> appBarActions = [];
}

class HomePage extends StatefulWidget {
  static final homeKey = GlobalKey<HomePageState>();

  HomePage() : super(key: homeKey);

  @override
  HomePageState createState() => HomePageState();
}

class HomePageState extends State<HomePage> with WidgetsBindingObserver {
  var _selectedIndex = 0;
  int get selectedIndex => _selectedIndex;
  final List<PageShape> _pages = [];
  int _chatPageTabIndex = -1;
  late final FirstRunPermissionFlow _firstRunPermissionFlow;
  bool get isChatPageCurrentTab => isAndroid
      ? _selectedIndex == _chatPageTabIndex
      : false; // change this when ios have chat page

  void refreshPages() {
    setState(() {
      initPages();
    });
  }

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    _firstRunPermissionFlow = FirstRunPermissionFlow(
      [
        () => _requestStandardPermissionsBatch(),
        () => _requestInputServicePermission(),
      ],
      stepNames: ['standard_permissions', 'input_service'],
      onStepProgress: (name, index, total, granted) {
        RuntimeLogger.instance.info('ANDROID',
            'first-run step ${index + 1}/$total ($name): ${granted ? "granted" : "denied/skipped"}');
      },
    );
    initPages();
    WidgetsBinding.instance.addPostFrameCallback((_) async {
      // Start the network layer early and unconditionally (idempotent). Even
      // if the first-run authorization flow is still running or was skipped,
      // peer online-state queries must be able to fire. Do NOT await it here:
      // the authorization flow below runs in parallel and may show dialogs.
      if (isAndroid && !bind.isOutgoingOnly()) {
        try {
          await gFFI.serverModel.startNetworkSilently();
        } catch (e) {
          RuntimeLogger.instance.info('ANDROID', 'early network start failed: $e');
        }
      }
      _runFirstLaunchAuthorization();
    });
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  /// True when the user has completed the one-time authorization flow (any of
  /// the persisted markers is present). Once marked, the app ALWAYS starts
  /// silently - accessibility is deliberately not re-checked here because it
  /// is a hosting capability only needed when this phone is remote-controlled.
  Future<bool> _canStartQuietly() async {
    // Every marker check is isolated: a transient failure of ONE marker (native
    // channel not ready yet, storage briefly unavailable, ...) must NEVER flip
    // the whole decision to "not authorized" - that would re-open the first-run
    // authorization window on an already-authorized device. Any hit => true.
    try {
      final nativePublic = await gFFI.invokeMethod('read_public_auth_marker');
      if (nativePublic == true) return true;
    } catch (e) {
      RuntimeLogger.instance.info('ANDROID', 'public marker check failed: $e');
    }
    try {
      if (bind.mainGetLocalOption(key: _kFirstRunAuthorization) == 'Y') {
        return true;
      }
    } catch (e) {
      RuntimeLogger.instance.info('ANDROID', 'local option check failed: $e');
    }
    try {
      final prefs = await gFFI.invokeMethod('get_first_run_authorization');
      if (prefs == true) return true;
    } catch (e) {
      RuntimeLogger.instance.info('ANDROID', 'prefs marker check failed: $e');
    }
    try {
      if (_readPublicAuthMarker()) {
        return true;
      }
    } catch (e) {
      RuntimeLogger.instance.info('ANDROID', 'legacy marker check failed: $e');
    }
    return false;
  }

  /// On every cold start:
  ///  - already-authorized device (incl. a reinstall, signalled by the public
  ///    marker that survives uninstall): start FULLY SILENT - no permission
  ///    dialog, no accessibility jump, no screen-capture prompt.  Runtime
  ///    permissions are requested on demand from the "Share screen" page when
  ///    the user actually enables each capability.
  ///  - first install: run the guided one-tap flow once (standard runtime
  ///    permissions only, no special settings pages).
  Future<void> _runFirstLaunchAuthorization() async {
    if (!isAndroid || bind.isOutgoingOnly() || !mounted) {
      return;
    }
    try {
      if (await _canStartQuietly()) {
        // Already authorized (any marker, incl. cross-reinstall public marker):
        // stay completely quiet.  Asking for permissions again here is exactly
        // what re-opens a system dialog on every launch after Android resets
        // runtime permissions on reinstall - never do that on cold start.
        RuntimeLogger.instance.info('ANDROID',
            'authorized device: silent cold start, no permission dialog');
        // Bring up the Rust network layer (peer registration + online-state
        // queries) on every cold start WITHOUT the share-screen permission
        // dialog. Previously the network only started from init_service after
        // the MediaProjection grant, so a phone that skipped the dialog could
        // never query peers and every remote device showed grey/offline.
        await gFFI.serverModel.startNetworkSilently();
        return;
      }
      RuntimeLogger.instance
          .info('ANDROID', 'first-run authorization sequence started');
      final completed = await _firstRunPermissionFlow.run();
      // Persist the marker regardless of which optional steps the user
      // skipped. Hosting capabilities (accessibility / screen capture) are
      // requested on demand in the "Share screen" page, so an incomplete
      // flow here must NOT re-trigger the full authorization window on the
      // next cold start.
      await bind.mainSetLocalOption(key: _kFirstRunAuthorization, value: 'Y');
      await gFFI.invokeMethod('set_first_run_authorization', true);
      _writePublicAuthMarker();
      // Native public marker survives reinstall (resolved by Kotlin).
      await gFFI.invokeMethod('write_public_auth_marker');
      RuntimeLogger.instance.info('ANDROID',
          'first-run authorization sequence finished (completed=$completed)');
    } catch (error, stackTrace) {
      RuntimeLogger.instance.error(
          'ANDROID', 'first-run authorization failed: $error\n$stackTrace');
    }
  }

  /// Batch-request the standard runtime permissions that matter for the core
  /// remote-control flow in ONE system dialog (they merge on modern Android).
  /// The "special" system pages (all-files access, battery-optimization list)
  /// are deliberately NOT requested here — each is a full separate settings
  /// screen and they are only needed by the optional file manager / keep-alive;
  /// they stay available in Settings when actually used.
  Future<bool> _requestStandardPermissionsBatch() async {
    final typesToRequest = <String>[];
    var allOk = true;

    // Notification (Android 13+)
    if (androidVersion >= 33 &&
        !await AndroidPermissionManager.check(kAndroid13Notification)) {
      typesToRequest.add(kAndroid13Notification);
    }
    // Record audio
    if (!await AndroidPermissionManager.check(kRecordAudio)) {
      typesToRequest.add(kRecordAudio);
    }

    if (typesToRequest.isEmpty) {
      return true;
    }

    // Request all ungranted standard permissions at once via batch API.
    RuntimeLogger.instance.info('ANDROID',
        'batch requesting ${typesToRequest.length} permissions: $typesToRequest');
    await gFFI.invokeMethod('request_permissions_batch', typesToRequest);

    // Wait for the batch dialog to complete by polling each permission.
    for (final type in typesToRequest) {
      // Poll up to 30 seconds for each permission to be granted
      var granted = false;
      for (var attempt = 0; attempt < 60; attempt++) {
        await Future<void>.delayed(const Duration(milliseconds: 500));
        if (await AndroidPermissionManager.check(type)) {
          granted = true;
          break;
        }
        if (!mounted) break;
      }
      if (!granted) allOk = false;
      RuntimeLogger.instance.info(
          'ANDROID', 'batch permission result; type=$type granted=$granted');
    }

    return allOk;
  }

  /// The one special capability Android will not let us grant ourselves: the
  /// "LDesk Input" accessibility service, the only way remote mouse / keyboard
  /// events can be injected into this device.
  ///
  /// Without it a remote session still shows the screen but silently ignores
  /// every input event - the exact "能看不能操作" symptom. The grant is also not
  /// durable: the ROM drops it after an app update or a force-stop on
  /// MIUI / EMUI / ColorOS. So ask for it once here, while the user is already
  /// walking through an authorization flow and the jump lands directly on the
  /// toggle, and let the "Share screen" page recover it if it is lost later.
  Future<bool> _requestInputServicePermission() async {
    if (!isAndroid) {
      return true;
    }
    try {
      if (await AndroidPermissionManager.checkAccessibility()) {
        RuntimeLogger.instance.info('ANDROID', 'input service already enabled');
        return true;
      }
      final go = await gFFI.dialogManager.show<bool>(
        tag: 'first-run-input-service',
        (setState, close, context) => CustomAlertDialog(
          title: null,
          content: Text(
            translate('android_input_permission_tip1'),
            style: const TextStyle(fontSize: 15),
          ),
          actions: [
            dialogButton('Later',
                onPressed: () => close(false), isOutline: true),
            dialogButton('Enable', onPressed: () => close(true)),
          ],
          onCancel: () => close(false),
        ),
      );
      if (go != true) {
        RuntimeLogger.instance
            .info('ANDROID', 'input service authorization skipped by user');
        return false;
      }
      await AndroidPermissionManager.startAction(
          kActionAccessibilityDetailsSettings);
      // Poll while the user is on the system page and stop as soon as the
      // toggle is flipped, so the flow never lingers after it succeeded.
      for (var attempt = 0; attempt < 40; attempt++) {
        await Future<void>.delayed(const Duration(milliseconds: 500));
        if (!mounted) {
          return false;
        }
        if (await AndroidPermissionManager.checkAccessibility()) {
          RuntimeLogger.instance
              .info('ANDROID', 'input service enabled by user');
          return true;
        }
      }
      RuntimeLogger.instance.info(
          'ANDROID', 'input service still not enabled; continuing without it');
      return false;
    } catch (e) {
      RuntimeLogger.instance
          .error('ANDROID', 'input service authorization failed: $e');
      return false;
    }
  }

  void initPages() {
    _pages.clear();
    if (!bind.isIncomingOnly()) {
      _pages.add(ConnectionPage(
        appBarActions: bind.isDisableSettings() ? [] : [_ScanConnectButton()],
      ));
    }
    if (isAndroid && !bind.isOutgoingOnly()) {
      _chatPageTabIndex = _pages.length;
      _pages.addAll([ChatPage(type: ChatPageType.mobileMain), ServerPage()]);
    }
    _pages.add(SettingsPage());
  }

  @override
  Widget build(BuildContext context) {
    return PopScope(
        canPop: _selectedIndex == 0,
        onPopInvokedWithResult: (didPop, result) {
          if (didPop) return;
          setState(() {
            _selectedIndex = 0;
          });
        },
        child: Scaffold(
          // backgroundColor: MyTheme.grayBg,
          appBar: AppBar(
            centerTitle: true,
            title: appTitle(),
            actions: _pages.elementAt(_selectedIndex).appBarActions,
          ),
          bottomNavigationBar: BottomNavigationBar(
            key: navigationBarKey,
            items: _pages
                .map((page) =>
                    BottomNavigationBarItem(icon: page.icon, label: page.title))
                .toList(),
            currentIndex: _selectedIndex,
            type: BottomNavigationBarType.fixed,
            selectedItemColor: MyTheme.accent, //
            unselectedItemColor: MyTheme.darkGray,
            onTap: (index) => setState(() {
              // close chat overlay when go chat page
              if (_selectedIndex != index) {
                _selectedIndex = index;
                if (isChatPageCurrentTab) {
                  gFFI.chatModel.hideChatIconOverlay();
                  gFFI.chatModel.hideChatWindowOverlay();
                  gFFI.chatModel.mobileClearClientUnread(
                      gFFI.chatModel.currentKey.connId);
                }
              }
            }),
          ),
          body: _pages.elementAt(_selectedIndex),
        ));
  }

  Widget appTitle() {
    final currentUser = gFFI.chatModel.currentUser;
    final currentKey = gFFI.chatModel.currentKey;
    if (isChatPageCurrentTab &&
        currentUser != null &&
        currentKey.peerId.isNotEmpty) {
      final connected =
          gFFI.serverModel.clients.any((e) => e.id == currentKey.connId);
      return Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          Tooltip(
            message: currentKey.isOut
                ? translate('Outgoing connection')
                : translate('Incoming connection'),
            child: Icon(
              currentKey.isOut
                  ? Icons.call_made_rounded
                  : Icons.call_received_rounded,
            ),
          ),
          Expanded(
            child: Center(
              child: Row(
                mainAxisAlignment: MainAxisAlignment.center,
                children: [
                  Text(
                    "${currentUser.firstName}   ${currentUser.id}",
                  ),
                  if (connected)
                    Container(
                      width: 10,
                      height: 10,
                      decoration: BoxDecoration(
                          shape: BoxShape.circle,
                          color: Color.fromARGB(255, 133, 246, 199)),
                    ).marginSymmetric(horizontal: 2),
                ],
              ),
            ),
          ),
        ],
      );
    }
    return Text(bind.mainGetAppNameSync());
  }
}

class WebHomePage extends StatelessWidget {
  final connectionPage =
      ConnectionPage(appBarActions: <Widget>[const WebSettingsPage()]);

  @override
  Widget build(BuildContext context) {
    stateGlobal.isInMainPage = true;
    handleUnilink(context);
    return Scaffold(
      // backgroundColor: MyTheme.grayBg,
      appBar: AppBar(
        centerTitle: true,
        title: Text("${bind.mainGetAppNameSync()} (${translate('Preview')})"),
        actions: connectionPage.appBarActions,
      ),
      body: connectionPage,
    );
  }

  handleUnilink(BuildContext context) {
    if (webInitialLink.isEmpty) {
      return;
    }
    final link = webInitialLink;
    webInitialLink = '';
    final splitter = ["/#/", "/#", "#/", "#"];
    var fakelink = '';
    for (var s in splitter) {
      if (link.contains(s)) {
        var list = link.split(s);
        if (list.length < 2 || list[1].isEmpty) {
          return;
        }
        list.removeAt(0);
        fakelink = "luoda://${list.join(s)}";
        break;
      }
    }
    if (fakelink.isEmpty) {
      return;
    }
    final uri = Uri.tryParse(fakelink);
    if (uri == null) {
      return;
    }
    final args = urlLinkToCmdArgs(uri);
    if (args == null || args.isEmpty) {
      return;
    }
    bool isFileTransfer = false;
    bool isViewCamera = false;
    bool isTerminal = false;
    String? id;
    String? password;
    for (int i = 0; i < args.length; i++) {
      switch (args[i]) {
        case '--connect':
        case '--play':
          id = args[i + 1];
          i++;
          break;
        case '--file-transfer':
          isFileTransfer = true;
          id = args[i + 1];
          i++;
          break;
        case '--view-camera':
          isViewCamera = true;
          id = args[i + 1];
          i++;
          break;
        case '--terminal':
          isTerminal = true;
          id = args[i + 1];
          i++;
          break;
        case '--terminal-admin':
          setEnvTerminalAdmin();
          isTerminal = true;
          id = args[i + 1];
          i++;
          break;
        case '--password':
          password = args[i + 1];
          i++;
          break;
        default:
          break;
      }
    }
    if (id != null) {
      connect(context, id,
          isFileTransfer: isFileTransfer,
          isViewCamera: isViewCamera,
          isTerminal: isTerminal,
          password: password);
    }
  }
}

/// AppBar button that opens the QR scanner for one-tap connect.
class _ScanConnectButton extends StatelessWidget {
  @override
  Widget build(BuildContext context) {
    return IconButton(
      icon: Icon(Icons.qr_code_scanner),
      tooltip: translate('Scan QR'),
      onPressed: () {
        Navigator.push(
          context,
          MaterialPageRoute(builder: (ctx) => ScanPage()),
        );
      },
    );
  }
}
