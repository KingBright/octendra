// Ordinary Gemini macOS AX bridge. All actions are scoped to an owned turn.
import Foundation
import AppKit
import ApplicationServices
import CoreGraphics
import ImageIO
import AVFoundation
import CryptoKit
import Darwin

let version = 1
let bundleID = "com.google.GeminiMacOS"
let maxText = 65_536
var operationDeadline = Date.distantFuture
func emit(_ v: [String: Any]) {
    guard JSONSerialization.isValidJSONObject(v),
          let data = try? JSONSerialization.data(withJSONObject: v, options: [.sortedKeys]),
          let s = String(data: data, encoding: .utf8) else { exit(1) }
    print(s); fflush(stdout)
}
func event(_ kind: String, _ reason: String, _ req: [String: Any], _ text: String? = nil,
           completed: Bool = false, notSubmitted: Bool = false, recovery: String? = nil, failureObserved: Bool = false,
           completionEvidence: [String:Any]? = nil, artifacts: [[String:Any]]? = nil) {
    var out: [String: Any] = ["version": version, "kind": kind, "reason": reason,
                              "completion_observed": completed]
    for k in ["request_id", "session_id"] { if let x = req[k] as? String { out[k] = x } }
    if let text = text { out["text"] = text; out["text_format"] = "plain_text" }
    if notSubmitted { out["not_submitted"] = true }
    if let recovery = recovery { out["recovery"] = recovery }
    if failureObserved { out["failure_observed"] = true }
    if let evidence = completionEvidence { out["completion_evidence"] = evidence }
    if let artifacts = artifacts { out["artifacts"] = artifacts }
    emit(out)
}
func attr(_ e: AXUIElement, _ name: String) -> (AXError, CFTypeRef?) {
    var value: CFTypeRef?
    let status = AXUIElementCopyAttributeValue(e, name as CFString, &value)
    return (status, value)
}
func str(_ e: AXUIElement, _ name: String) -> String { attr(e, name).1 as? String ?? "" }
// Unsupported/no-value attributes are genuinely absent; all transient AX
// failures invalidate the observation rather than pretending a field is empty.
func metadataValue(_ status: AXError, _ value: String?) -> String? {
    if status == .attributeUnsupported || status == .noValue { return "" }
    guard status == .success else { return nil }
    return value
}
func metadata(_ e: AXUIElement, _ name: String) -> String? {
    let (status,value) = attr(e,name)
    return metadataValue(status,value as? String)
}
let newChatLabels: Set<String> = ["发起新对话","新建任务","New chat","Start new chat","New task","Start new task"]
let chatComposerNames: Set<String> = ["问问 Gemini","Ask Gemini","接下来做什么？","What’s next?","What's next?","描述任务","分配任务","Message Gemini","Describe a task","Describe your task","Assign a task"]
let composerNames: Set<String> = chatComposerNames.union(["描述要创作的图片","描述要创作的视频","你想研究什么？",
    "一起开始创作吧","Describe your image","Describe your video","What do you want to research?",
    "Let’s write or build together"])
func validatedMetadata(_ roleStatus: AXError, _ roleValue: String?,
                       _ idStatus: AXError, _ idValue: String?,
                       _ descStatus: AXError, _ descValue: String?,
                       passiveLeaf: Bool = false) -> (String,String,String)? {
    guard roleStatus == .success, let role = roleValue, !role.isEmpty else { return nil }
    // Critical interactive/marker roles (`AXButton`, `AXHeading`, `AXTextArea`) must
    // fail closed on AXError.failure unless they are a known composer or passive leaf.
    let criticalControl = ["AXButton", "AXHeading", "AXTextArea"].contains(role)
    let desc = metadataValue(descStatus, descValue) ?? (!criticalControl && descStatus == .failure ? "" : nil)
    guard let resolvedDesc = desc else { return nil }
    let knownComposer = role == "AXTextArea" && composerNames.contains(resolvedDesc)
    let knownPassive = role == "AXGroup" && passiveLeaf
    let allowMissingId = knownComposer || knownPassive || !criticalControl
    guard let resolvedId = metadataValue(idStatus, idValue) ?? (allowMissingId && idStatus == .failure ? "" : nil) else {
        return nil
    }
    return (role, resolvedId, resolvedDesc)
}
func isDraftControl(_ role: String, _ desc: String, _ ident: String, help: String? = nil) -> Bool {
    guard role == "AXButton" else { return false }
    if ["共享屏幕","Share screen"].contains(desc) {
        // In 1.141 the same unselected action is permanently present. Only
        // the exact packaged help proves it is the generic chooser.
        return !["选择“屏幕上的内容”选项。","Select screen context options."].contains(help ?? "")
    }
    return ["移除","Remove","Screen context","Screen sharing","移除附件","删除附件",
            "Remove attachment","Remove file"].contains(desc) || ident.hasPrefix("remove-attachment")
}
func passiveReadOnlyTextArea(_ role: String?, _ idStatus: AXError, _ descStatus: AXError,
                             _ safeChildren: Bool, _ actionStatus: AXError, _ actions: [String]?,
                             _ valueStatus: AXError, _ value: String?, _ setStatus: AXError, _ settable: Bool) -> Bool {
    return role == "AXTextArea" && idStatus == .failure && descStatus == .failure && safeChildren &&
           actionStatus == .success && actions == ["AXShowMenu"] && valueStatus == .success && value != nil &&
           setStatus == .success && !settable
}
func passiveTextLeaf(_ role: String?, _ idStatus: AXError, _ desc: String?,
                     _ childCount: Int, _ actionStatus: AXError, _ actions: [String]?,
                     _ valueStatus: AXError, _ value: String?) -> Bool {
    return role == "AXGroup" && idStatus == .failure && childCount == 0 &&
           actionStatus == .success && actions == ["AXShowMenu"] && valueStatus == .success &&
           desc != nil && !desc!.isEmpty && value == desc
}
func children(_ e: AXUIElement) -> [AXUIElement]? {
    let (s, value) = attr(e, kAXChildrenAttribute)
    if s == .attributeUnsupported || s == .noValue { return [] }
    guard s == .success else { return nil }
    return value as? [AXUIElement]
}
struct Node {
    var element: AXUIElement
    var role: String
    var ident: String
    var desc: String
    var parent: Int
}
func tree(_ window: AXUIElement) -> [Node]? {
    // The 1.141 main window includes volatile sidebar conversation cells.
    // The observed chat panel is w.0.0.2; the separately observed New Chat
    // button is w.0.0.0.3. Unknown layouts fail closed.
    let (sheetStatus,sheetValue) = attr(window,"AXSheets")
    if sheetStatus == .success {
        guard let sheets = sheetValue as? [AXUIElement], sheets.isEmpty else { return nil }
    } else if sheetStatus != .noValue && sheetStatus != .attributeUnsupported { return nil }
    guard let currentApp = app() else { return nil }
    let (focusStatus,focusValue) = attr(AXUIElementCreateApplication(currentApp.processIdentifier),kAXFocusedWindowAttribute)
    if focusStatus == .success, let focused = focusValue, CFGetTypeID(focused) == AXUIElementGetTypeID() {
        let focusedWin = focused as! AXUIElement
        if !CFEqual(focusedWin, window) && isGeminiChatWindow(focusedWin) { return nil }
    } else {
        guard let ws = windows(currentApp), let sole = standardWindow(ws), CFEqual(sole,window) else { return nil }
    }
    guard let first = children(window), !first.isEmpty,
          let second = children(first[0]), !second.isEmpty,
          let regions = children(second[0]), regions.count > 2,
          let sidebar = children(regions[0]), sidebar.count > 3 else { return nil }
    let panel = regions[2], newChat = sidebar[3]
    // AXSheets is unsupported by 1.141. Reject sheets/dialogs in the window
    // frame and content ancestors; the full chat subtree is inspected below.
    var frame: [(AXUIElement,Int)] = [(window,0)]
    while !frame.isEmpty {
        let (item,depth) = frame.removeLast()
        if CFEqual(item,panel) { continue }
        let (roleStatus,roleValue) = attr(item,kAXRoleAttribute)
        guard roleStatus == .success, let role = roleValue as? String,
              !["AXSheet","AXDialog"].contains(role) else { return nil }
        if depth < 3 {
            guard let cs = children(item) else { return nil }
            for child in cs { frame.append((child,depth+1)) }
        }
    }
    guard str(newChat,kAXRoleAttribute) == "AXButton",
          newChatLabels.contains(str(newChat,kAXDescriptionAttribute)) else { return nil }
    var nodes = [Node(element:window,role:"AXWindow",ident:"",desc:"",parent:-1),
                 Node(element:panel,role:"",ident:"",desc:"",parent:0),
                 Node(element:newChat,role:"",ident:"",desc:"",parent:0)]
    var i = 1
    let chromeRoles: Set<String> = ["AXUnknown", "AXScrollBar", "AXValueIndicator", "AXSplitter", "AXHandle"]
    while i < nodes.count {
        if nodes.count > 12000 || Date() >= operationDeadline { return nil }
        let e = nodes[i].element
        AXUIElementSetMessagingTimeout(e, 0.25)
        var (roleStatus,roleValue) = attr(e,kAXRoleAttribute)
        if i > 2 && [AXError.invalidUIElement, .cannotComplete, .failure].contains(roleStatus) {
            nodes[i].role = "AXUnknown"; nodes[i].ident = ""; nodes[i].desc = ""
            i += 1
            continue
        }
        var (idStatus,idValue) = attr(e,kAXIdentifierAttribute)
        var (descStatus,descValue) = attr(e,kAXDescriptionAttribute)
        var csOpt = children(e)
        if i > 2 && csOpt == nil && roleStatus == .success {
            usleep(15_000)
            (roleStatus,roleValue) = attr(e,kAXRoleAttribute)
            (idStatus,idValue) = attr(e,kAXIdentifierAttribute)
            (descStatus,descValue) = attr(e,kAXDescriptionAttribute)
            csOpt = children(e)
        }
        let cs: [AXUIElement]
        if let unwrapped = csOpt {
            cs = unwrapped
        } else if i > 2 && roleStatus == .success {
            cs = []
        } else {
            return nil
        }
        func evaluateNode() -> (String, String, String)? {
            var safeLeaf = false
            if idStatus == .failure && (roleValue as? String) == "AXGroup" && cs.isEmpty {
                var rawActions: CFArray?
                let actionStatus = AXUIElementCopyActionNames(e,&rawActions)
                let (valueStatus,value) = attr(e,kAXValueAttribute)
                safeLeaf = passiveTextLeaf(roleValue as? String,idStatus,descValue as? String,cs.count,
                                           actionStatus,rawActions as? [String],valueStatus,value as? String)
            }
            var passiveArea = false
            if roleStatus == .success && (roleValue as? String) == "AXTextArea" &&
               idStatus == .failure && descStatus == .failure && cs.count <= 1 {
                var rawActions: CFArray?
                let actionStatus = AXUIElementCopyActionNames(e,&rawActions)
                let (valueStatus,value) = attr(e,kAXValueAttribute)
                var settable = DarwinBoolean(false)
                let setStatus = AXUIElementIsAttributeSettable(e,kAXValueAttribute as CFString,&settable)
                let safeChildren = cs.isEmpty || (cs.count == 1 && {
                    let (status,value) = attr(cs[0],kAXRoleAttribute)
                    return status == .success && (value as? String) == "AXUnknown"
                }())
                passiveArea = passiveReadOnlyTextArea(roleValue as? String,idStatus,descStatus,safeChildren,
                                                     actionStatus,rawActions as? [String],valueStatus,value as? String,
                                                     setStatus,settable.boolValue)
            }
            let validated = validatedMetadata(roleStatus,roleValue as? String,idStatus,idValue as? String,
                                              descStatus,descValue as? String,passiveLeaf:safeLeaf)
            return validated ?? (passiveArea ? ("AXTextArea","","") : nil)
        }
        var resolvedNode = evaluateNode()
        if resolvedNode == nil && i > 2 {
            usleep(15_000)
            (roleStatus,roleValue) = attr(e,kAXRoleAttribute)
            (idStatus,idValue) = attr(e,kAXIdentifierAttribute)
            (descStatus,descValue) = attr(e,kAXDescriptionAttribute)
            resolvedNode = evaluateNode()
        }
        if resolvedNode == nil && i > 2 {
            let roleStr = (roleValue as? String) ?? ""
            let transientErr: Set<AXError> = [.invalidUIElement, .cannotComplete]
            if roleStr.isEmpty || (chromeRoles.contains(roleStr) && (transientErr.contains(roleStatus) || transientErr.contains(idStatus) || transientErr.contains(descStatus))) {
                nodes[i].role = "AXUnknown"; nodes[i].ident = ""; nodes[i].desc = ""
                i += 1
                continue
            }
        }
        guard let (role,id,desc) = resolvedNode else { return nil }
        nodes[i].role = role
        nodes[i].ident = id
        nodes[i].desc = desc
        for c in cs { nodes.append(Node(element:c,role:"",ident:"",desc:"",parent:i)) }
        i += 1
    }
    return nodes
}
func inside(_ nodes: [Node], _ child: Int, _ ancestor: Int) -> Bool {
    var p = child
    while p >= 0 { if p == ancestor { return true }; p = nodes[p].parent }
    return false
}
func ancestor(_ nodes: [Node], _ index: Int, role: String) -> Int? {
    var i = index
    while i >= 0 { if nodes[i].role == role { return i }; i = nodes[i].parent }
    return nil
}
func isUserHeading(_ nodes: [Node], _ index: Int) -> Bool {
    guard nodes[index].role == "AXHeading" else { return false }
    if ["userMessage.expandButton","userMessage.expandButton-userMessage.expandButton"].contains(nodes[index].ident) { return true }
    // Gemini 1.141 omits the identifier. Accept only its observed
    // row -> cell -> group -> sole heading shape, never answer body headings.
    guard nodes[index].ident.isEmpty else { return false }
    let group = nodes[index].parent
    guard group >= 0, nodes[group].role == "AXGroup" else { return false }
    let cell = nodes[group].parent
    guard cell >= 0, nodes[cell].role == "AXCell" else { return false }
    let row = nodes[cell].parent
    guard row >= 0, nodes[row].role == "AXRow" else { return false }
    return nodes.indices.filter({nodes[$0].parent == group}).count == 1 &&
           nodes.indices.filter({nodes[$0].parent == cell}).count == 1 &&
           nodes.indices.filter({nodes[$0].parent == row}).count == 1 &&
           !nodes.contains(where:{$0.parent == index})
}
func enabled(_ e: AXUIElement) -> Bool { attr(e, kAXEnabledAttribute).1 as? Bool == true }
func press(_ e: AXUIElement) -> Bool { AXUIElementPerformAction(e, kAXPressAction as CFString) == .success }
func lock() -> Int32? {
    let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Caches/GeminiDesktopMCP")
    do { try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700]) }
    catch { return nil }
    let fd = open(dir.appendingPathComponent("gemini-desktop.lock").path, O_CREAT | O_RDWR | O_NOFOLLOW | O_CLOEXEC, 0o600)
    guard fd >= 0 else { return nil }
    var st = stat()
    guard fstat(fd, &st) == 0, (st.st_mode & S_IFMT) == S_IFREG, st.st_uid == getuid(),
          flock(fd, LOCK_EX | LOCK_NB) == 0 else { close(fd); return nil }
    return fd
}
func unlock(_ fd: Int32) { flock(fd, LOCK_UN); close(fd) }
func app() -> NSRunningApplication? {
    let a = NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
    return a.count == 1 ? a[0] : nil
}
func ensureAppAndWindow() -> (NSRunningApplication, AXUIElement)? {
    if let a = app(), let ws = windows(a), let w = standardWindow(ws) {
        return (a, w)
    }
    guard let appURL = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) else {
        return nil
    }
    let cfg = NSWorkspace.OpenConfiguration()
    cfg.activates = false
    cfg.addsToRecentItems = false
    let sem = DispatchSemaphore(value: 0)
    NSWorkspace.shared.openApplication(at: appURL, configuration: cfg) { _, _ in sem.signal() }
    _ = sem.wait(timeout: .now() + 4.0)
    let waitUntil = min(operationDeadline, Date().addingTimeInterval(4.5))
    while Date() < waitUntil {
        if let a = app(), let ws = windows(a), let w = standardWindow(ws) {
            return (a, w)
        }
        if let a = app(), (windows(a)?.isEmpty ?? true) {
            let root = AXUIElementCreateApplication(a.processIdentifier)
            let (mStatus, mValue) = attr(root, kAXMenuBarAttribute)
            if mStatus == .success, let rawMenu = mValue, CFGetTypeID(rawMenu) == AXUIElementGetTypeID(),
               let newWinItem = uniqueControl(rawMenu as! AXUIElement, roles: ["AXMenuItem"], labels: ["New Window", "新建窗口", "New Gemini Window"]) {
                _ = press(newWinItem)
            }
        }
        _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
    }
    if let a = app(), let ws = windows(a), let w = standardWindow(ws) {
        return (a, w)
    }
    return nil
}
func isAlreadyBlankChat(_ ui: UI) -> Bool {
    return ui.stop == nil && ui.draft.isEmpty && !ui.dialog && !ui.hasAttachment &&
           !ui.nodes.indices.contains(where: { isUserHeading(ui.nodes, $0) || ui.nodes[$0].ident == "copy-button" })
}
func clearStaleSelectedTool(_ window: AXUIElement, _ ui: UI) -> UI? {
    guard ui.selectedTool else { return ui }
    let removeButtons = ui.nodes.indices.filter { i in
        ui.nodes[i].role == "AXButton" && ["移除工具", "Remove tool"].contains(ui.nodes[i].desc) && enabled(ui.nodes[i].element)
    }
    guard removeButtons.count == 1, press(ui.nodes[removeButtons[0]].element) else { return nil }
    let deadline = min(operationDeadline, Date().addingTimeInterval(2.5))
    while Date() < deadline {
        _ = CFRunLoopRunInMode(.defaultMode, 0.2, false)
        if let refreshed = snapshot(window), !refreshed.selectedTool {
            return refreshed
        }
    }
    return snapshot(window)
}
func windows(_ a: NSRunningApplication) -> [AXUIElement]? {
    let root = AXUIElementCreateApplication(a.processIdentifier)
    AXUIElementSetMessagingTimeout(root, 0.7)
    let (s, v) = attr(root, kAXWindowsAttribute)
    guard s == .success else { return nil }
    return v as? [AXUIElement]
}
func isGeminiChatWindow(_ window: AXUIElement) -> Bool {
    guard let first = children(window), !first.isEmpty,
          let second = children(first[0]), !second.isEmpty,
          let regions = children(second[0]), regions.count > 2,
          let sidebar = children(regions[0]), sidebar.count > 3 else { return false }
    let newChat = sidebar[3]
    return str(newChat, kAXRoleAttribute) == "AXButton" &&
           newChatLabels.contains(str(newChat, kAXDescriptionAttribute))
}
func hasFrameDialog(_ window: AXUIElement, panel: AXUIElement) -> Bool {
    let (sheetStatus, sheetValue) = attr(window, "AXSheets")
    if sheetStatus == .success, let sheets = sheetValue as? [AXUIElement], !sheets.isEmpty { return true }
    var frame: [(AXUIElement, Int)] = [(window, 0)]
    while let (item, depth) = frame.popLast() {
        if CFEqual(item, panel) { continue }
        let role = str(item, kAXRoleAttribute)
        if ["AXSheet", "AXDialog"].contains(role) { return true }
        if depth < 3, let cs = children(item) {
            for child in cs { frame.append((child, depth + 1)) }
        }
    }
    return false
}
func lightweightNewChatHandle(_ window: AXUIElement) -> (ok: Bool, reason: String, button: AXUIElement?) {
    guard let first = children(window), !first.isEmpty,
          let second = children(first[0]), !second.isEmpty,
          let regions = children(second[0]), regions.count > 2,
          let sidebar = children(regions[0]), sidebar.count > 3 else {
        return (false, "busy:ui_state_unavailable", nil)
    }
    let newChat = sidebar[3]
    guard str(newChat, kAXRoleAttribute) == "AXButton",
          newChatLabels.contains(str(newChat, kAXDescriptionAttribute)),
          enabled(newChat) else {
        return (false, "new_chat_control_unavailable", nil)
    }
    let panel = regions[2]
    if hasFrameDialog(window, panel: panel) { return (false, "busy:dialog_present", nil) }
    var queue: [(AXUIElement, Int)] = [(panel, 0)]
    var qIdx = 0
    var foundInputs = 0
    while qIdx < queue.count && qIdx < 64 {
        let (cur, depth) = queue[qIdx]; qIdx += 1
        AXUIElementSetMessagingTimeout(cur, 0.15)
        let cRole = str(cur, kAXRoleAttribute)
        if ["AXOutline", "AXTable", "AXList"].contains(cRole) { continue }
        if depth > 0 {
            let cDesc = str(cur, kAXDescriptionAttribute)
            let cIdent = str(cur, kAXIdentifierAttribute)
            if isDraftControl(cRole, cDesc, cIdent, help: metadata(cur, kAXHelpAttribute)) {
                return (false, "busy:draft_present", nil)
            }
            if cRole == "AXButton" && ["移除工具", "Remove tool"].contains(cDesc) {
                // Allowed on fresh run; post-newChat blank verification clears any stale tool chip.
            }
            if cRole == "AXTextArea" && composerNames.contains(cDesc) {
                foundInputs += 1
                let (vs, vv) = attr(cur, kAXValueAttribute)
                guard vs == .success, let draft = vv as? String else { return (false, "busy:ui_state_unavailable", nil) }
                if !draft.isEmpty { return (false, "busy:draft_present", nil) }
                continue
            }
        }
        if depth < 3, let cs = children(cur) {
            for child in cs.prefix(16) {
                queue.append((child, depth + 1))
            }
        }
    }
    guard foundInputs == 1 else { return (false, "busy:ui_state_unavailable", nil) }
    return (true, "ready", newChat)
}
func standardWindow(_ ws: [AXUIElement]) -> AXUIElement? {
    var standard: [AXUIElement] = []
    for w in ws {
        let (status,value) = attr(w,kAXSubroleAttribute)
        guard status == .success, let subrole = value as? String else { continue }
        if subrole == "AXStandardWindow" { standard.append(w); continue }
        let (modalStatus,modal) = attr(w,kAXModalAttribute)
        if modalStatus == .success && (modal as? Bool) == true { return nil }
    }
    let chatWins = standard.filter { isGeminiChatWindow($0) }
    if let firstChat = chatWins.first { return firstChat }
    return standard.count == 1 ? standard[0] : standard.first
}
// Map AX geometry to one unique layer-zero CG window number. No window title is stored.
func windowID(_ w: AXUIElement, _ pid: pid_t) -> UInt32? {
    guard let p = attr(w, kAXPositionAttribute).1, CFGetTypeID(p) == AXValueGetTypeID(),
          let z = attr(w, kAXSizeAttribute).1, CFGetTypeID(z) == AXValueGetTypeID() else { return nil }
    var pos = CGPoint.zero, size = CGSize.zero
    guard AXValueGetValue(p as! AXValue, .cgPoint, &pos),
          AXValueGetValue(z as! AXValue, .cgSize, &size) else { return nil }
    let list = CGWindowListCopyWindowInfo([.optionAll, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    let matches = list.filter { item in
        guard (item[kCGWindowOwnerPID as String] as? Int32) == pid,
              [0, 3, 8].contains((item[kCGWindowLayer as String] as? Int) ?? -1),
              let b = item[kCGWindowBounds as String] as? [String: CGFloat],
              let x = b["X"], let y = b["Y"], let width = b["Width"], let height = b["Height"] else { return false }
        return abs(x-pos.x) < 1 && abs(y-pos.y) < 1 && abs(width-size.width) < 1 && abs(height-size.height) < 1
    }
    if matches.count == 1, let num = matches[0][kCGWindowNumber as String] as? UInt32 {
        return num
    }
    if matches.isEmpty, isGeminiChatWindow(w),
       let a = NSRunningApplication(processIdentifier: pid),
       let ws = windows(a), ws.filter({ isGeminiChatWindow($0) }).count == 1 {
        return UInt32(bitPattern: pid) &+ 0x4000_0000
    }
    return nil
}
struct UI {
    let nodes: [Node]
    let input: Int
    let stop: Int?
    let newChat: Int?
    let draft: String
    let dialog: Bool
    var hasAttachment: Bool = false
    var selectedTool: Bool = false
}
func snapshot(_ w: AXUIElement) -> UI? {
    guard let ns = tree(w) else { return nil }
    let inputs = ns.indices.filter { ns[$0].role == "AXTextArea" && composerNames.contains(ns[$0].desc) }
    guard inputs.count == 1 else { return nil }
    let (status, value) = attr(ns[inputs[0]].element, kAXValueAttribute)
    guard status == .success, let draft = value as? String else { return nil }
    let stops = ns.indices.filter { ns[$0].role == "AXButton" && (ns[$0].ident == "stop_button" || ["停止回答","Stop response"].contains(ns[$0].desc)) }
    let news = ns.indices.filter { ns[$0].role == "AXButton" && newChatLabels.contains(ns[$0].desc) }
    guard stops.count <= 1, news.count <= 1 else { return nil }
    let dialog = ns.contains { ["AXDialog","AXSheet"].contains($0.role) }
    let inputParent = ns[inputs[0]].parent
    let composerPanel = inputParent >= 0 ? ns[inputParent].parent : -1
    let attachment = ns.indices.contains { index in
        composerPanel >= 0 && inside(ns,index,composerPanel) &&
            isDraftControl(ns[index].role,ns[index].desc,ns[index].ident,
                help:metadata(ns[index].element,kAXHelpAttribute))
    }
    let selectedTools = ns.indices.filter { index in
        composerPanel >= 0 && inside(ns,index,composerPanel) && ns[index].role == "AXButton" &&
        ["移除工具","Remove tool"].contains(ns[index].desc)
    }
    guard selectedTools.count <= 1 else { return nil }
    return UI(nodes: ns, input: inputs[0], stop: stops.first, newChat: news.first,
              draft: draft, dialog: dialog, hasAttachment: attachment, selectedTool: !selectedTools.isEmpty)
}
struct Turn {
    let owned: Bool
    let text: String?
    let copy: Bool
    let waiting: Bool
    var tooLarge: Bool = false
    var readFailed: Bool = false
}
let answerControlButtonLabels: Set<String> = [
    "来源", "Sources", "展开", "显示更多", "Show more", "Expand", "收起", "Show less", "Collapse",
    "开始研究", "Start research", "Canvas", "重新生成", "Regenerate", "更多操作", "More actions",
    "复制", "复制回答", "复制代码", "Copy", "Copy response", "Copy code",
    "答得好", "答得不好", "Good response", "Bad response",
    "允许一次", "Allow once", "允许在此对话中使用", "Allow for this chat", "允许", "Allow",
    "不允许", "Don’t allow", "Don't allow", "拒绝", "Deny", "以后再说", "Not now",
]
func cleanAnswerButtonText(_ raw: String) -> String {
    raw.replacingOccurrences(of: "\u{FFFC}", with: "").trimmingCharacters(in: .whitespacesAndNewlines)
}
func answerProseButtonValue(_ ns: [Node], _ index: Int, reader: ((Int) -> String?)? = nil) -> String? {
    guard ns[index].role == "AXButton", ns[index].ident.isEmpty,
          !ns.contains(where: { $0.parent == index }) else { return nil }
    let trimmedDesc = ns[index].desc.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !trimmedDesc.isEmpty, !answerControlButtonLabels.contains(trimmedDesc) else { return nil }
    if let reader = reader {
        guard let raw = reader(index) else { return nil }
        let cleaned = cleanAnswerButtonText(raw)
        return cleaned.isEmpty ? nil : cleaned
    }
    let (status, raw) = attr(ns[index].element, kAXValueAttribute)
    guard status == .success, let strVal = raw as? String else { return nil }
    let cleaned = cleanAnswerButtonText(strVal)
    guard !cleaned.isEmpty, !["已展开", "已收起", "Expanded", "Collapsed"].contains(cleaned) else { return nil }
    return cleaned
}
// Clean prompt normalization & ordered turn matching without injecting visible request IDs.
func normalizeHeadingText(_ s: String) -> String {
    let folded = s
        .components(separatedBy: .whitespacesAndNewlines)
        .filter { !$0.isEmpty }
        .joined(separator: " ")
    return folded.trimmingCharacters(in: .whitespacesAndNewlines)
}
func isLegacyMarker(_ expected: String) -> Bool {
    expected.hasPrefix("[GeminiDesktop request ")
}
func headingMatchesExpected(_ headingDesc: String, _ expected: String) -> Bool {
    if isLegacyMarker(expected) {
        return headingDesc.contains(expected)
    }
    let normH = normalizeHeadingText(headingDesc)
    let normE = normalizeHeadingText(expected)
    guard !normH.isEmpty, !normE.isEmpty else { return false }
    if normH == normE { return true }
    var trimmedH = normH
    if trimmedH.hasSuffix("…") {
        trimmedH.removeLast()
    } else if trimmedH.hasSuffix("...") {
        trimmedH.removeLast(3)
    }
    trimmedH = trimmedH.trimmingCharacters(in: .whitespacesAndNewlines)
    if trimmedH != normH {
        if trimmedH == normE { return true }
        if trimmedH.count >= 12 && normE.hasPrefix(trimmedH) { return true }
    }
    if normH.count >= 24 && normE.hasPrefix(normH) { return true }
    return false
}
func chatSiblingRowCount(_ ui: UI) -> Int? {
    let ns = ui.nodes
    if let firstHeading = ns.indices.first(where: { isUserHeading(ns, $0) }),
       let row = ancestor(ns, firstHeading, role: "AXRow"),
       ns[row].parent >= 0 {
        let p = ns[row].parent
        return ns.indices.filter { ns[$0].parent == p && ns[$0].role == "AXRow" }.count
    }
    return nil
}
// Its next AXRow alone may own a response. Unknown layouts fail closed.
func turn(_ ui: UI, marker: String, history: [String]? = nil, allowDraft: Bool = false, reader: ((Int) -> String?)? = nil) -> Turn {
    let ns = ui.nodes
    let expected = history ?? [marker]
    let user = ns.indices.filter { isUserHeading(ns,$0) }
    let hasLegacy = expected.contains(where: { isLegacyMarker($0) })
    guard !expected.isEmpty, expected.last == marker,
          (!hasLegacy || Set(expected).count == expected.count),
          !user.isEmpty, !ui.hasAttachment, (allowDraft || ui.draft.isEmpty) else {
        return Turn(owned: false, text: nil, copy: false, waiting: ui.dialog)
    }
    var sharedParent: Int? = nil
    var siblings: [Int] = []
    var ordered: [(Int,Int)] = []
    for heading in user {
        guard let userRow = ancestor(ns,heading,role:"AXRow"), ns[userRow].parent >= 0 else {
            return Turn(owned:false,text:nil,copy:false,waiting:ui.dialog)
        }
        let p = ns[userRow].parent
        if let existingParent = sharedParent {
            guard existingParent == p else {
                return Turn(owned:false,text:nil,copy:false,waiting:ui.dialog)
            }
        } else {
            sharedParent = p
            siblings = ns.indices.filter { ns[$0].parent == p && ns[$0].role == "AXRow" }
        }
        guard let offset = siblings.firstIndex(of:userRow) else {
            return Turn(owned:false,text:nil,copy:false,waiting:ui.dialog)
        }
        ordered.append((offset,heading))
    }
    ordered.sort { $0.0 < $1.0 }
    guard Set(ordered.map { $0.0 }).count == ordered.count else {
        return Turn(owned:false,text:nil,copy:false,waiting:ui.dialog)
    }
    let windowMatchesExact = ordered.count == expected.count &&
        zip(ordered,expected).allSatisfy({ pair,mark in headingMatchesExpected(ns[pair.1].desc, mark) })
    let windowMatchesVirtualized = expected.count > 12 && ordered.count >= 2 && ordered.count < expected.count &&
        zip(ordered,expected.suffix(ordered.count)).allSatisfy({ pair,mark in headingMatchesExpected(ns[pair.1].desc, mark) })
    guard (windowMatchesExact || windowMatchesVirtualized),
          let (offset, targetHeading) = ordered.last,
          headingMatchesExpected(ns[targetHeading].desc, marker) else {
        return Turn(owned:false,text:nil,copy:false,waiting:ui.dialog)
    }
    let following = offset + 1 < siblings.count ? siblings[offset+1] : nil
    guard let answer = following else { return Turn(owned: true, text: nil, copy: false, waiting: ui.dialog) }
    let nested = ns.indices.filter { inside(ns, $0, answer) }
    // The next user message is not an answer.
    if nested.contains(where: { isUserHeading(ns,$0) }) {
        return Turn(owned: false, text: nil, copy: false, waiting: ui.dialog)
    }
    let copy = nested.contains { ns[$0].role == "AXButton" && ns[$0].ident == "copy-button" }
    let waits = Set(["tool-consent-allow", "tool-consent-deny", "quota-dialog", "account-dialog"])
    let yes = Set(["允许一次", "Allow once", "允许在此对话中使用", "Allow for this chat", "允许", "Allow"])
    let no = Set(["不允许", "Don’t allow", "Don't allow", "拒绝", "Deny", "以后再说", "Not now"])
    let consentPair = nested.filter { ns[$0].role == "AXGroup" }.contains { group in
        let controls = nested.filter { ns[$0].role == "AXButton" && inside(ns,$0,group) }
        return controls.contains { yes.contains(ns[$0].desc) } && controls.contains { no.contains(ns[$0].desc) }
    }
    let waiting = ui.dialog || consentPair || nested.contains { waits.contains(ns[$0].ident) }
    // Answer body is the sibling of the action-button group within the answer row.
    // In Gemini 1.144.4+, short/sourced answers can be exposed as leaf AXButton prose nodes
    // beside the actions group and a separate "来源" ("Sources") button.
    let copyIndices = nested.filter { ns[$0].role == "AXButton" && ns[$0].ident == "copy-button" }
    var bodyRoots: [Int] = []
    if let copyIndex = copyIndices.first, let actions = ancestor(ns, copyIndex, role: "AXGroup") {
        let p = ns[actions].parent
        let candidates = nested.filter { ns[$0].parent == p && $0 != actions &&
            ["AXGroup","AXStaticText","AXTextArea"].contains(ns[$0].role) }
        if candidates.count == 1 {
            bodyRoots = [candidates[0]]
        } else {
            bodyRoots = nested.filter { idx in
                guard ns[idx].parent == p && idx != actions else { return false }
                if ["AXGroup","AXStaticText","AXTextArea","AXHeading","AXLink"].contains(ns[idx].role) {
                    return true
                }
                if ns[idx].role == "AXButton" {
                    return answerProseButtonValue(ns, idx, reader: reader) != nil
                }
                return false
            }
        }
    }
    guard !bodyRoots.isEmpty else { return Turn(owned: true, text: nil, copy: copy, waiting: waiting) }
    // Traversal must preserve presentation order, unlike breadth-first node storage.
    var parts: [String] = []
    var totalBytes = 0, tooLarge = false, readFailed = false
    func collect(_ i: Int) {
        if tooLarge || readFailed { return }
        let children = ns.indices.filter { ns[$0].parent == i }
        if ["AXStaticText","AXTextArea"].contains(ns[i].role) ||
           (children.isEmpty && ["AXHeading","AXLink"].contains(ns[i].role)) {
            var value: String
            if let reader = reader {
                guard let result = reader(i) else { readFailed = true; return }
                value = result
            } else {
                let (status, raw) = attr(ns[i].element,kAXValueAttribute)
                if status == .success, let result = raw as? String {
                    value = result
                } else if [.attributeUnsupported, .noValue, .failure].contains(status),
                          ["AXHeading","AXLink"].contains(ns[i].role) {
                    if let title = metadata(ns[i].element, kAXTitleAttribute), !title.isEmpty {
                        value = title
                    } else if !ns[i].desc.isEmpty {
                        value = ns[i].desc
                    } else {
                        readFailed = true; return
                    }
                } else if [.attributeUnsupported, .noValue, .failure].contains(status) {
                    if let title = metadata(ns[i].element, kAXTitleAttribute), !title.isEmpty {
                        value = title
                    } else if !ns[i].desc.isEmpty {
                        value = ns[i].desc
                    } else if status == .attributeUnsupported || status == .noValue {
                        value = ""
                    } else {
                        readFailed = true; return
                    }
                } else {
                    readFailed = true; return
                }
            }
            if value.isEmpty && ["AXHeading","AXLink"].contains(ns[i].role) { value = ns[i].desc }
            if value.contains("\u{FFFC}") {
                value = cleanAnswerButtonText(value)
            }
            if !value.isEmpty {
                totalBytes += value.utf8.count + (parts.isEmpty ? 0 : 1)
                if totalBytes > maxText { tooLarge = true } else { parts.append(value) }
            }
            return
        }
        if ns[i].role == "AXButton" {
            if let btnText = answerProseButtonValue(ns, i, reader: reader) {
                totalBytes += btnText.utf8.count + (parts.isEmpty ? 0 : 1)
                if totalBytes > maxText { tooLarge = true } else { parts.append(btnText) }
            }
            return
        }
        for child in children { collect(child) }
    }
    for root in bodyRoots { collect(root) }
    // AX exposes rendered text fragments, not the original Markdown. Preserve
    // their presentation order, joining blocks with newlines; expanded content
    // that is not actually visible must not be reported as complete.
    let collapsed = nested.contains { ns[$0].role == "AXButton" &&
        ["展开","显示更多","Show more","Expand"].contains(ns[$0].desc) }
    let text = collapsed || tooLarge || readFailed ? "" : parts.joined(separator: "\n")
    return Turn(owned: true, text: text.isEmpty ? nil : text,
                copy: copy, waiting: waiting, tooLarge: tooLarge && !collapsed,
                readFailed: readFailed)
}
func streamingPreviewText(_ ui: UI, marker: String) -> String? {
    guard let row = ownedAnswerRow(ui, marker, true) else { return nil }
    let ns = ui.nodes
    var parts: [String] = []
    var totalBytes = 0
    func walk(_ i: Int) {
        if totalBytes > maxText { return }
        let children = ns.indices.filter { ns[$0].parent == i }
        if ["AXStaticText", "AXTextArea"].contains(ns[i].role) ||
           (children.isEmpty && ["AXHeading", "AXLink"].contains(ns[i].role)) {
            let (status, raw) = attr(ns[i].element, kAXValueAttribute)
            var value = (status == .success ? (raw as? String) : nil) ?? ""
            if value.isEmpty && ["AXHeading", "AXLink"].contains(ns[i].role) {
                value = metadata(ns[i].element, kAXTitleAttribute) ?? ns[i].desc
            }
            if value.contains("\u{FFFC}") { value = cleanAnswerButtonText(value) }
            if !value.isEmpty {
                totalBytes += value.utf8.count + (parts.isEmpty ? 0 : 1)
                if totalBytes <= maxText { parts.append(value) }
            }
            return
        }
        if ns[i].role == "AXButton" {
            if let btnText = answerProseButtonValue(ns, i) {
                totalBytes += btnText.utf8.count + (parts.isEmpty ? 0 : 1)
                if totalBytes <= maxText { parts.append(btnText) }
            }
            return
        }
        for child in children { walk(child) }
    }
    walk(row)
    return parts.isEmpty ? nil : parts.joined(separator: "\n")
}
struct ImageArtifact: Codable {
    let artifactID: String
    let kind: String
    let mimeType: String
    let path: String
    let byteLength: Int64
    let sha256: String
    let requestID: String
    let sessionID: String
    enum CodingKeys: String, CodingKey {
        case artifactID = "artifact_id", kind, mimeType = "mime_type", path,
             byteLength = "byte_length", sha256, requestID = "request_id", sessionID = "session_id"
    }
    func wire() -> [String:Any] {
        return ["artifact_id":artifactID,"kind":kind,"mime_type":mimeType,"path":path,
                "byte_length":byteLength,"sha256":sha256,"request_id":requestID,"session_id":sessionID]
    }
}
struct Binding: Codable {
    let requestID: String
    let sessionID: String
    let marker: String
    let pid: Int32
    let launch: Double
    let window: UInt32
    var phase: String
    var sawStop: Bool
    var markerHistory: [String]? = nil
    var promptText: String? = nil
    var promptHistory: [String]? = nil
    var sidebarTitle: String? = nil
    var preSubmitRowCount: Int? = nil
    var priorRequestID: String? = nil
    var capability: String? = nil
    var requestedModel: String? = nil
    var requestedEffort: String? = nil
    var finalReason: String? = nil
    var finalText: String? = nil
    var finalCompletion: Bool? = nil
    var finalFailure: Bool? = nil
    var confirmationAttemptedVersion: String? = nil
    var confirmationPerformed: Bool? = nil
    var priorResearchApprovalVersion: String? = nil
    var canvasBlankVerified: Bool? = nil
    var priorCanvasTextSHA: String? = nil
    var canvasCloseAttempted: Bool? = nil
    var canvasClosePerformed: Bool? = nil
    var canvasCloseVerified: Bool? = nil
    var canvasOpenAttempted: Bool? = nil
    var canvasOpenPerformed: Bool? = nil
    var finalCanvasTitle: String? = nil
    var finalCanvasTextSHA: String? = nil
    var finalCanvasLocalID: String? = nil
    var mediaExportPhase: String? = nil
    var mediaExportPath: String? = nil
    var mediaExportID: String? = nil
    var finalExportProvenance: String? = nil
    var artifacts: [ImageArtifact]? = nil

    var matchTarget: String { promptText ?? marker }
    var matchHistory: [String] { promptHistory ?? markerHistory ?? [matchTarget] }
}
// A fresh chat must remain blank, while a continuation must still display
// the completed parent turn. Draft text and attachments are separate.
func chatMarker(_ session: String, _ requestID: String) -> String {
    let digest = SHA256.hash(data:Data(requestID.utf8)).map { String(format:"%02x",$0) }.joined()
    return "[GeminiDesktop request " + session + " " + String(digest.prefix(16)) + "]"
}
func provenChatBinding(_ b: Binding) -> Bool {
    if b.capability == "chat" { return true }
    // Legacy native bindings did not persist a capability, but only supported
    // chat and used this unique request-specific marker/history format.
    return b.capability == nil && b.markerHistory?.last == b.marker &&
           b.marker == chatMarker(b.sessionID,b.requestID)
}
func provenPriorBinding(_ b: Binding) -> Bool {
    return (b.phase == "completed" && terminalReceipt(b)?.completion == true) ||
           (b.phase == "failed" && b.capability == "image" && b.finalFailure == true &&
            b.finalCompletion == false && b.finalReason == "native_error_response" && b.finalText == knownImageAccessError)
}
func chatModeSelected(_ ui: UI, inheritedChat: Bool = false, selectedReader: ((Int) -> Bool?)? = nil) -> Bool {
    guard chatComposerNames.contains(ui.nodes[ui.input].desc), !ui.hasAttachment, !ui.selectedTool else { return false }
    let modes = ui.nodes.indices.filter {
        ui.nodes[$0].role == "AXButton" && ["chat-input-mode-ask","chat-input-mode-assign"].contains(ui.nodes[$0].ident)
    }
    // The Ask/Assign selector exists in a fresh chat but disappears after the
    // first owned response. A verified prior chat binds that existing context.
    if modes.isEmpty { return inheritedChat || modelSelectorControl(ui.nodes) != nil }
    if let selectedReader = selectedReader { return modes.contains { selectedReader($0) == true } }
    return modes.contains { idx in
        let (status,value) = attr(ui.nodes[idx].element,kAXSelectedAttribute)
        return status == .success && (value as? Bool) == true
    }
}
func viewSignature(_ ui: UI) -> [String] {
    return ui.nodes.indices.filter { isUserHeading(ui.nodes,$0) }.map {
        SHA256.hash(data:Data(ui.nodes[$0].desc.utf8)).map { String(format:"%02x",$0) }.joined()
    }
}
func newChatContextOK(_ current: UI, _ original: UI, prior: Binding? = nil,
                      selectedReader: ((Int) -> Bool?)? = nil) -> Bool {
    guard CFEqual(current.nodes[current.input].element,original.nodes[original.input].element),
          current.draft.isEmpty, !current.hasAttachment, current.stop == nil, !current.dialog,
          current.selectedTool == original.selectedTool, viewSignature(current) == viewSignature(original) else { return false }
    if let old = prior {
        guard sendContextOK(current,"",old,mode:(old.capability ?? "chat"),selectedReader:selectedReader) else { return false }
        if old.phase == "failed" {
            let observed = turn(current,marker:old.matchTarget,history:old.matchHistory)
            return decision(observed,stop:false) == .completed && observed.text == old.finalText
        }
        return true
    }
    return chatModeSelected(current,selectedReader:selectedReader)
}
func sendContextOK(_ ui: UI, _ expectedDraft: String, _ previous: Binding?, mode: String = "chat",
                   reader: ((Int) -> String?)? = nil, selectedReader: ((Int) -> Bool?)? = nil) -> Bool {
    guard !ui.hasAttachment, !ui.dialog, ui.stop == nil, ui.draft == expectedDraft else { return false }
    if mode == "chat" {
        guard chatModeSelected(ui,inheritedChat:previous.map { provenChatBinding($0) } ?? false,
                               selectedReader:selectedReader) else { return false }
    } else if mode == "image" {
        guard ui.selectedTool, ["描述要创作的图片","Describe your image"].contains(ui.nodes[ui.input].desc) else { return false }
    } else if mode == "canvas" {
        guard ui.selectedTool, ["一起开始创作吧","Let’s write or build together"].contains(ui.nodes[ui.input].desc) else { return false }
    } else if mode == "video" {
        guard ui.selectedTool, ["描述要创作的视频","Describe your video"].contains(ui.nodes[ui.input].desc) else { return false }
    } else if mode == "deep_research" {
        guard ui.selectedTool, ["你想研究什么？","What do you want to research?"].contains(ui.nodes[ui.input].desc) else { return false }
    } else { return false }
    if let old = previous {
        let prior = turn(ui,marker:old.matchTarget,history:old.matchHistory,allowDraft:true,reader:reader)
        guard prior.owned, prior.copy, !prior.readFailed, !prior.waiting else { return false }
        if ["canvas","deep_research"].contains(old.capability ?? "") {
            guard let title = old.finalCanvasTitle else { return false }
            return canvasDocument(old,ui,title,allowDraft:true) == old.finalText
        }
        if ["image","video"].contains(old.capability ?? "") { return true }
        return decision(prior,stop:false) == .completed && !(prior.text ?? "").isEmpty
    }
    return !ui.nodes.indices.contains { isUserHeading(ui.nodes,$0) || ui.nodes[$0].ident == "copy-button" }
}
func sidebarOutline(_ window: AXUIElement) -> AXUIElement? {
    guard let first = children(window), !first.isEmpty,
          let second = children(first[0]), !second.isEmpty,
          let regions = children(second[0]), !regions.isEmpty else { return nil }
    var queue: [AXUIElement] = [regions[0]]
    var idx = 0
    while idx < queue.count && idx < 256 {
        let cur = queue[idx]; idx += 1
        AXUIElementSetMessagingTimeout(cur, 0.1)
        if str(cur, kAXRoleAttribute) == "AXOutline" {
            return cur
        }
        if let cs = children(cur) {
            queue.append(contentsOf: cs)
        }
    }
    return nil
}
func sidebarRowTitle(_ row: AXUIElement) -> String? {
    var queue: [AXUIElement] = [row]
    var idx = 0
    var candidates: [String] = []
    while idx < queue.count && idx < 48 {
        let cur = queue[idx]; idx += 1
        AXUIElementSetMessagingTimeout(cur, 0.08)
        let role = str(cur, kAXRoleAttribute)
        if role == "AXStaticText" {
            if let v = attr(cur, kAXValueAttribute).1 as? String {
                let cleaned = normalizeHeadingText(v)
                if !cleaned.isEmpty { candidates.append(cleaned) }
            }
        }
        for k in [kAXTitleAttribute, kAXDescriptionAttribute] {
            let s = normalizeHeadingText(str(cur, k))
            if !s.isEmpty { candidates.append(s) }
        }
        if let cs = children(cur) {
            queue.append(contentsOf: cs)
        }
    }
    return candidates.first
}
func selectedSidebarConversationTitle(_ window: AXUIElement) -> String? {
    guard let outline = sidebarOutline(window), let rows = children(outline) else { return nil }
    let excluded: Set<String> = ["库", "图片", "已排定", "学生", "Library", "Images", "Scheduled", "Students", "Gems", "全部"]
    for row in rows {
        guard str(row, kAXRoleAttribute) == "AXRow" else { continue }
        let (selSt, selVal) = attr(row, kAXSelectedAttribute)
        guard selSt == .success, (selVal as? Bool) == true else { continue }
        if let title = sidebarRowTitle(row), !excluded.contains(title) {
            return title
        }
    }
    return nil
}
func sidebarRowMatches(_ title: String, target: String) -> Bool {
    let a = normalizeHeadingText(title)
    let b = normalizeHeadingText(target)
    guard !a.isEmpty, !b.isEmpty else { return false }
    if a == b { return true }
    return headingMatchesExpected(a, b) || (a.count >= 6 && b.hasPrefix(a))
}
func tryReselectPriorConversation(_ window: AXUIElement, old: Binding) -> UI? {
    guard let outline = sidebarOutline(window), let rows = children(outline) else { return nil }
    let excluded: Set<String> = ["库", "图片", "已排定", "学生", "Library", "Images", "Scheduled", "Students", "Gems", "全部"]
    var targets: [String] = []
    if let st = old.sidebarTitle, !st.isEmpty { targets.append(st) }
    if let firstPrompt = old.matchHistory.first, !firstPrompt.isEmpty { targets.append(firstPrompt) }
    guard !targets.isEmpty else { return nil }
    var matchedRows: [AXUIElement] = []
    for row in rows {
        guard str(row, kAXRoleAttribute) == "AXRow",
              let title = sidebarRowTitle(row), !excluded.contains(title) else { continue }
        if targets.contains(where: { sidebarRowMatches(title, target: $0) }) {
            matchedRows.append(row)
        }
    }
    guard matchedRows.count == 1 else { return nil }
    let clicked = press(matchedRows[0]) ||
        (AXUIElementSetAttributeValue(matchedRows[0], kAXSelectedAttribute as CFString, kCFBooleanTrue) == .success)
    guard clicked else { return nil }
    _ = CFRunLoopRunInMode(.defaultMode, 0.35, false)
    for _ in 0..<5 {
        if let ui = snapshot(window), sendContextOK(ui, "", old, mode: (old.capability ?? "chat")) {
            return ui
        }
        _ = CFRunLoopRunInMode(.defaultMode, 0.2, false)
    }
    return nil
}
// The image menu item was observed under the opened native tool picker in
// 1.141. A role/label/uniqueness check is required on every activation.
func modeMenuItem(_ window: AXUIElement, _ mode: String) -> AXUIElement? {
    let labels: [String]
    switch mode {
    case "image": labels = ["生成图片","Create image","Create images"]
    case "video": labels = ["生成视频","Create video","Create videos"]
    case "deep_research": labels = ["Deep Research"]
    case "canvas": labels = ["Canvas"]
    default: labels = []
    }
    guard !labels.isEmpty else { return nil }
    guard let a = children(window), !a.isEmpty, let b = children(a[0]), !b.isEmpty,
          let regions = children(b[0]), regions.count > 2, let panelChildren = children(regions[2]) else { return nil }
    var roots: [AXUIElement] = []
    for e in panelChildren {
        let (status,value) = attr(e,kAXRoleAttribute)
        guard status == .success, let role = value as? String else { return nil }
        if role == "AXMenu" { roots.append(e) }
    }
    guard roots.count == 1 else { return nil }
    var queue = roots, index = 0, matches: [AXUIElement] = []
    while index < queue.count && index < 256 {
        let e = queue[index]; index += 1
        let (roleStatus,roleValue) = attr(e,kAXRoleAttribute)
        guard roleStatus == .success, let role = roleValue as? String else { return nil }
        if role == "AXMenuItem" {
            guard let desc = metadata(e,kAXDescriptionAttribute), let title = metadata(e,kAXTitleAttribute) else { return nil }
            if labels.contains(desc) || labels.contains(title) { matches.append(e) }
        }
        guard let cs = children(e) else { return nil }
        queue.append(contentsOf:cs)
    }
    guard index == queue.count, matches.count == 1, enabled(matches[0]) else { return nil }
    return matches[0]
}
let discoveredModelsCacheFile = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent("Library/Caches/GeminiDesktopMCP/discovered-models.json").path

struct CachedDesktopModel: Codable {
    let id: String
    let label: String?
    let picker_token: String?
    let picker_label: String?
}

struct CachedDesktopModelsDoc: Codable {
    let discovered_at: Int64
    let models: [CachedDesktopModel]
}

func readCachedDesktopModelsRaw() -> (Int64, [[String: Any]])? {
    let fd = open(discoveredModelsCacheFile, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return nil }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0, (st.st_mode & S_IFMT) == S_IFREG, st.st_uid == getuid(),
          (st.st_mode & 0o077) == 0, st.st_size > 0, st.st_size < 262_144 else { return nil }
    let data = FileHandle(fileDescriptor: fd, closeOnDealloc: false).readDataToEndOfFile()
    guard let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
          let models = obj["models"] as? [[String: Any]] else { return nil }
    let ts = (obj["discovered_at"] as? NSNumber)?.int64Value ?? 0
    return (ts, models)
}

func writeCachedDesktopModels(_ models: [[String: Any]]) {
    let parent = (discoveredModelsCacheFile as NSString).deletingLastPathComponent
    if !FileManager.default.fileExists(atPath: parent) {
        try? FileManager.default.createDirectory(atPath: parent, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    }
    let payload: [String: Any] = [
        "discovered_at": Int64(Date().timeIntervalSince1970),
        "models": models,
    ]
    guard JSONSerialization.isValidJSONObject(payload),
          let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]) else { return }
    let temp = discoveredModelsCacheFile + "." + UUID().uuidString + ".tmp"
    let fd = open(temp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
    guard fd >= 0 else { return }
    let ok = data.withUnsafeBytes { p -> Bool in
        guard let base = p.baseAddress else { return false }
        return write(fd, base, p.count) == p.count
    }
    close(fd)
    if ok {
        if rename(temp, discoveredModelsCacheFile) != 0 { _ = unlink(temp) }
    } else {
        _ = unlink(temp)
    }
}

func extractDynamicModelToken(_ slugInput: String) -> String? {
    var s = slugInput.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    if s.isEmpty || s.contains("unknown") { return nil }
    for prefix in ["google/", "gemini_app/", "gemini-app/", "gemini/"] {
        if s.hasPrefix(prefix) {
            s = String(s.dropFirst(prefix.count))
            break
        }
    }
    if s.hasPrefix("gemini-") {
        let rest = String(s.dropFirst(7))
        if rest.isEmpty || rest == "desktop-task" || rest == "desktop-auto" { return nil }
        let parts = rest.split(separator: "-", maxSplits: 1).map(String.init)
        if parts.count == 2,
           let firstScalar = parts[0].unicodeScalars.first,
           CharacterSet.decimalDigits.contains(firstScalar),
           !parts[1].isEmpty,
           parts[1].allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") }) {
            return parts[1]
        }
        if rest.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "." || $0 == "-") }) {
            return rest
        }
        return nil
    }
    let spaceParts = s.split(separator: " ", maxSplits: 1).map(String.init)
    if spaceParts.count == 2,
       spaceParts[0].allSatisfy({ $0.isASCII && ($0.isNumber || $0 == ".") }),
       !spaceParts[1].isEmpty,
       spaceParts[1].allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") }) {
        return spaceParts[1]
    }
    if let (_, cached) = readCachedDesktopModelsRaw() {
        for item in cached {
            let cid = (item["id"] as? String ?? "").lowercased()
            let token = (item["picker_token"] as? String ?? "").lowercased()
            if s == cid || s == token {
                if cid.hasPrefix("gemini-"), let extracted = extractDynamicModelToken(cid) {
                    return extracted
                }
                return token.isEmpty ? s : token
            }
        }
    }
    return nil
}

func canonicalDesktopModel(_ raw: String?) -> String? {
    guard let raw = raw else { return "" }
    var slug = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    if slug.isEmpty { return "" }
    for prefix in ["google/", "gemini_app/", "gemini-app/", "gemini/"] {
        if slug.hasPrefix(prefix) {
            slug = String(slug.dropFirst(prefix.count))
            break
        }
    }
    for suffix in ["-minimal", "-low", "-medium", "-high", "-xhigh", "-max"] {
        if slug.hasSuffix(suffix) && slug.count > suffix.count {
            slug = String(slug.dropLast(suffix.count))
            break
        }
    }
    switch slug {
    case "auto", "default", "gemini-desktop-task": return ""
    case "gemini-desktop-auto": return "desktop_auto"
    case "flash", "hydrogen", "gemini-4-hydrogen", "gemini-4-flash", "gemini-flash": return "flash"
    case "argon", "gemini-4-argon": return "argon"
    case "carbon", "gemini-4-carbon", "pro", "gemini-pro": return "carbon"
    default: return extractDynamicModelToken(slug)
    }
}
func canonicalDesktopEffort(_ raw: String?) -> String? {
    guard let raw = raw else { return "" }
    let slug = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    if slug.isEmpty { return "" }
    switch slug {
    case "auto", "default": return ""
    case "minimal", "none", "off": return "minimal"
    case "low", "standard": return "low"
    case "medium", "med": return "medium"
    case "high", "extended": return "high"
    case "xhigh", "extra_high", "x-high": return "xhigh"
    case "max", "deep", "deep_think", "deep-think": return "max"
    default: return nil
    }
}
func modelDisplayCandidates(_ canonicalModel: String) -> [String] {
    var out: [String]
    switch canonicalModel {
    case "desktop_auto": out = ["自动", "auto"]
    case "flash": out = ["flash", "hydrogen", "4 hydrogen"]
    case "argon": out = ["argon", "4 argon"]
    case "carbon": out = ["carbon", "4 carbon"]
    default: out = canonicalModel.isEmpty ? [] : [canonicalModel, "4 " + canonicalModel, "5 " + canonicalModel]
    }
    if !canonicalModel.isEmpty, let (_, cached) = readCachedDesktopModelsRaw() {
        for item in cached {
            let cid = (item["id"] as? String ?? "").lowercased()
            let token = (item["picker_token"] as? String ?? "").lowercased()
            if (!cid.isEmpty && (cid.hasSuffix("-" + canonicalModel) || cid == canonicalModel))
                || (!token.isEmpty && (token == canonicalModel || token.hasSuffix(" " + canonicalModel))) {
                if !token.isEmpty && !out.contains(token) { out.append(token) }
            }
        }
    }
    return out
}
func effortDisplayCandidates(_ canonicalEffort: String) -> [String] {
    switch canonicalEffort {
    case "minimal": return ["minimal", "精简", "极简", "off", "关闭", "none"]
    case "low": return ["low", "standard", "低", "标准", "快速"]
    case "medium": return ["medium", "中", "中等", "均衡"]
    case "high": return ["high", "extended", "高", "扩展", "深入"]
    case "xhigh": return ["extra high", "xhigh", "超高", "极高", "extended", "扩展", "deep think", "深度思考"]
    case "max": return ["max", "deep think", "最大", "极限", "深度思考", "深度"]
    default: return []
    }
}
func selectorLabelPrefix(_ desc: String) -> String {
    for suffix in ["，选择模型", ", Choose model"] {
        if desc.hasSuffix(suffix) {
            return String(desc.dropLast(suffix.count)).trimmingCharacters(in: .whitespacesAndNewlines)
        }
    }
    return desc.trimmingCharacters(in: .whitespacesAndNewlines)
}
func modelSelectorControl(_ nodes: [Node]) -> Int? {
    let matches = nodes.indices.filter { i in
        nodes[i].role == "AXButton" &&
        (nodes[i].ident == "tab_stop_model_selector" ||
         nodes[i].desc.hasSuffix("，选择模型") ||
         nodes[i].desc.hasSuffix(", Choose model"))
    }
    return matches.count == 1 ? matches[0] : nil
}
func selectorMatchesModel(_ desc: String, canonicalModel: String) -> Bool {
    if canonicalModel.isEmpty { return true }
    let lower = selectorLabelPrefix(desc).lowercased()
    return modelDisplayCandidates(canonicalModel).contains { lower.contains($0) }
}
func selectorMatchesEffort(_ desc: String, canonicalEffort: String) -> Bool {
    if canonicalEffort.isEmpty { return true }
    let lower = selectorLabelPrefix(desc).lowercased()
    return effortDisplayCandidates(canonicalEffort).contains { lower.contains($0) }
}
func pickerTextSegments(_ raw: String) -> [String] {
    let separators = CharacterSet(charactersIn: "、,，·•")
    return raw.lowercased()
        .components(separatedBy: separators)
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        .filter { !$0.isEmpty }
}
func pickerTextsMatch(_ texts: [String], candidates: [String]) -> (exact: Bool, prefixOrSegment: Bool) {
    guard let primary = texts.first else { return (false, false) }
    if candidates.contains(where: { primary == $0 || texts.contains($0) }) {
        return (true, false)
    }
    let allSegments = texts.flatMap { pickerTextSegments($0) }
    if candidates.contains(where: { cand in allSegments.contains(cand) }) {
        return (true, false)
    }
    if candidates.contains(where: { cand in
        primary.hasPrefix(cand + " ") || primary.hasPrefix(cand + "，") || primary.hasPrefix(cand + ",") || primary.hasPrefix(cand + "、")
            || allSegments.contains(where: { $0.hasSuffix(" " + cand) || $0.hasPrefix(cand + " ") })
    }) {
        return (false, true)
    }
    return (false, false)
}
func parseDiscoveredModelOption(_ texts: [String]) -> [String: Any]? {
    let submenuHeaders: Set<String> = ["思考等级", "thinking level", "minimal", "low", "medium", "high", "extra high", "max"]
    let lowerTexts = texts.map { $0.lowercased().trimmingCharacters(in: .whitespacesAndNewlines) }
    guard let primaryRaw = texts.first?.trimmingCharacters(in: .whitespacesAndNewlines), !primaryRaw.isEmpty else { return nil }
    if lowerTexts.contains(where: { submenuHeaders.contains($0) }) { return nil }

    let separators = CharacterSet(charactersIn: "、,，")
    let rawSegs = primaryRaw.components(separatedBy: separators)
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        .filter { !$0.isEmpty }

    let isAuto = rawSegs.first.map { ["自动", "auto"].contains($0.lowercased()) } ?? false
    if isAuto {
        let desc = rawSegs.count > 1 ? rawSegs.dropFirst().joined(separator: "、") : primaryRaw
        return [
            "id": "gemini-desktop-auto",
            "label": "自动 · \(desc) (gemini-desktop-auto)",
            "picker_label": primaryRaw,
            "picker_token": rawSegs[0],
            "default_effort": NSNull(),
            "supported_efforts": [String](),
        ]
    }

    var tokenSeg: String? = nil
    var descSegs: [String] = []
    for seg in rawSegs {
        let words = seg.split(separator: " ").map(String.init)
        let startsWithVer = words.count >= 2 && words[0].allSatisfy({ $0.isASCII && ($0.isNumber || $0 == ".") })
        let startsWithGemini = words.first?.lowercased() == "gemini" && words.count >= 2
        if tokenSeg == nil && (startsWithVer || startsWithGemini) {
            tokenSeg = seg
        } else {
            descSegs.append(seg)
        }
    }

    let efforts = ["low", "medium", "high", "xhigh", "max"]
    if let tok = tokenSeg {
        let words = tok.split(separator: " ").map(String.init)
        let normWords = words.map { w -> String in
            w.lowercased().filter { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == ".") }
        }.filter { !$0.isEmpty }
        let baseParts = normWords.first == "gemini" ? Array(normWords.dropFirst()) : normWords
        let modelId = "gemini-" + baseParts.joined(separator: "-")
        let dispTitle = normWords.first == "gemini" ? tok : "Gemini \(tok)"
        let descText = descSegs.joined(separator: "、")
        let label = descText.isEmpty ? "\(dispTitle) (\(modelId))" : "\(dispTitle) · \(descText) (\(modelId))"
        let defEff = modelId.contains("carbon") ? "high" : "medium"
        return [
            "id": modelId,
            "label": label,
            "picker_label": primaryRaw,
            "picker_token": tok,
            "default_effort": defEff,
            "supported_efforts": efforts,
        ]
    }

    let asciiSlug = primaryRaw.lowercased().map { ch -> Character in
        if ch.isASCII && (ch.isLetter || ch.isNumber || ch == ".") { return ch }
        return "-"
    }
    let collapsed = String(asciiSlug).split(separator: "-").map(String.init).joined(separator: "-")
    guard !collapsed.isEmpty else { return nil }
    let fallbackId = collapsed.hasPrefix("gemini-") ? collapsed : "gemini-\(collapsed)"
    return [
        "id": fallbackId,
        "label": "\(primaryRaw) (\(fallbackId))",
        "picker_label": primaryRaw,
        "picker_token": primaryRaw,
        "default_effort": "medium",
        "supported_efforts": efforts,
    ]
}
func discoverDesktopModels(forceRefresh: Bool, installed: Bool, trusted: Bool) -> [[String: Any]] {
    let nowSec = Int64(Date().timeIntervalSince1970)
    let cached = readCachedDesktopModelsRaw()
    if !forceRefresh, let (ts, models) = cached, !models.isEmpty, nowSec - ts < 600 {
        return models
    }
    guard installed, trusted, let a = app(), let ws = windows(a), let mainWin = standardWindow(ws) else {
        return cached?.1 ?? []
    }
    var leaseStat = stat()
    if lstat(leaseFile, &leaseStat) == 0 || errno != ENOENT {
        return cached?.1 ?? []
    }
    guard let fd = lock() else { return cached?.1 ?? [] }
    defer { unlock(fd) }
    reclaimStaleLeaseIfSafe(mainWin)
    if lstat(leaseFile, &leaseStat) == 0 || errno != ENOENT {
        return cached?.1 ?? []
    }
    if let qs = quickStatus(mainWin), qs.busy {
        return cached?.1 ?? []
    }
    guard let ui = snapshot(mainWin), ui.stop == nil, ui.draft.isEmpty, !ui.dialog, !ui.hasAttachment,
          let selIdx = modelSelectorControl(ui.nodes), enabled(ui.nodes[selIdx].element) else {
        return cached?.1 ?? []
    }
    guard press(ui.nodes[selIdx].element) else { return cached?.1 ?? [] }
    _ = CFRunLoopRunInMode(.defaultMode, 0.38, false)
    let roots = pickerContainers(a, mainWin)
    var discovered: [[String: Any]] = []
    var seenIds: Set<String> = []
    let actionableRoles: Set<String> = ["AXButton", "AXMenuItem", "AXRadioButton", "AXCheckBox", "AXCell", "AXRow", "AXGroup"]
    for root in roots {
        var queue = [root]
        var idx = 0
        while idx < queue.count && idx < 256 {
            let el = queue[idx]; idx += 1
            let role = str(el, kAXRoleAttribute)
            if actionableRoles.contains(role) {
                var rawActions: CFArray?
                let actStatus = AXUIElementCopyActionNames(el, &rawActions)
                let actions = (actStatus == .success ? (rawActions as? [String]) : nil) ?? []
                let canActivate = actions.contains(kAXPressAction as String) || actions.contains("AXPick") || ["AXButton", "AXMenuItem", "AXRadioButton"].contains(role)
                if canActivate && (attr(el, kAXEnabledAttribute).1 as? Bool ?? true) {
                    let texts = collectElementTexts(el)
                    if let parsed = parseDiscoveredModelOption(texts),
                       let mid = parsed["id"] as? String, !seenIds.contains(mid) {
                        seenIds.insert(mid)
                        discovered.append(parsed)
                    }
                }
            }
            if let cs = children(el) { queue.append(contentsOf: cs) }
        }
    }
    dismissPickerIfOpen(a.processIdentifier)
    _ = CFRunLoopRunInMode(.defaultMode, 0.15, false)
    if !discovered.isEmpty {
        writeCachedDesktopModels(discovered)
        return discovered
    }
    return cached?.1 ?? []
}
func pickerContainers(_ a: NSRunningApplication, _ mainWindow: AXUIElement) -> [AXUIElement] {
    var roots: [AXUIElement] = []
    let appRoot = AXUIElementCreateApplication(a.processIdentifier)
    if let appChildren = children(appRoot) {
        for ch in appChildren {
            let role = str(ch, kAXRoleAttribute)
            if ["AXPopover", "AXMenu"].contains(role) {
                roots.append(ch)
            } else if role == "AXWindow" && !CFEqual(ch, mainWindow) {
                let subrole = str(ch, kAXSubroleAttribute)
                if subrole != "AXStandardWindow" { roots.append(ch) }
            }
        }
    }
    if let ws = windows(a) {
        for candidate in ws where !CFEqual(candidate, mainWindow) {
            let subrole = str(candidate, kAXSubroleAttribute)
            if subrole != "AXStandardWindow" && !roots.contains(where: { CFEqual($0, candidate) }) {
                roots.append(candidate)
            }
        }
    }
    var queue: [(AXUIElement, Int)] = [(mainWindow, 0)]
    var idx = 0
    while idx < queue.count && idx < 1200 {
        let (el, depth) = queue[idx]; idx += 1
        let role = str(el, kAXRoleAttribute)
        if ["AXPopover", "AXMenu"].contains(role) {
            if !roots.contains(where: { CFEqual($0, el) }) { roots.append(el) }
            continue
        }
        if depth < 10, let cs = children(el) {
            for c in cs { queue.append((c, depth + 1)) }
        }
    }
    return roots
}
func collectElementTexts(_ element: AXUIElement) -> [String] {
    var out: [String] = []
    for key in [kAXTitleAttribute, kAXDescriptionAttribute, kAXValueAttribute] {
        if let v = attr(element, key).1 as? String {
            let trimmed = v.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty { out.append(trimmed) }
        }
    }
    var queue: [(AXUIElement, Int)] = [(element, 0)]
    var idx = 0
    while idx < queue.count && idx < 32 {
        let (cur, depth) = queue[idx]; idx += 1
        if depth > 0 {
            let role = str(cur, kAXRoleAttribute)
            if ["AXStaticText", "AXHeading"].contains(role) {
                for key in [kAXValueAttribute, kAXTitleAttribute, kAXDescriptionAttribute] {
                    if let v = attr(cur, key).1 as? String {
                        let trimmed = v.trimmingCharacters(in: .whitespacesAndNewlines)
                        if !trimmed.isEmpty { out.append(trimmed) }
                    }
                }
            }
        }
        if depth < 3, let cs = children(cur) {
            for c in cs { queue.append((c, depth + 1)) }
        }
    }
    return out
}
func findPickerItem(_ roots: [AXUIElement], candidates: [String], allowSubmenuHeader: Bool = false) -> AXUIElement? {
    let actionableRoles: Set<String> = ["AXButton", "AXMenuItem", "AXRadioButton", "AXCheckBox", "AXCell", "AXRow", "AXGroup"]
    let submenuHeaders: Set<String> = ["思考等级", "thinking level"]
    var exactMatches: [AXUIElement] = []
    var prefixMatches: [AXUIElement] = []
    for root in roots {
        var queue = [root]
        var idx = 0
        while idx < queue.count && idx < 256 {
            let el = queue[idx]; idx += 1
            let role = str(el, kAXRoleAttribute)
            if actionableRoles.contains(role) {
                var rawActions: CFArray?
                let actStatus = AXUIElementCopyActionNames(el, &rawActions)
                let actions = (actStatus == .success ? (rawActions as? [String]) : nil) ?? []
                let canActivate = actions.contains(kAXPressAction as String) || actions.contains("AXPick") || ["AXButton", "AXMenuItem", "AXRadioButton"].contains(role)
                if canActivate && (attr(el, kAXEnabledAttribute).1 as? Bool ?? true) {
                    let texts = collectElementTexts(el).map { $0.lowercased() }
                    if !texts.isEmpty {
                        let isHeader = texts.contains { submenuHeaders.contains($0) }
                        if allowSubmenuHeader || !isHeader {
                            let m = pickerTextsMatch(texts, candidates: candidates)
                            if m.exact {
                                exactMatches.append(el)
                            } else if m.prefixOrSegment {
                                prefixMatches.append(el)
                            }
                        }
                    }
                }
            }
            if let cs = children(el) { queue.append(contentsOf: cs) }
        }
    }
    if let first = exactMatches.first { return first }
    return prefixMatches.first
}
func dismissPickerIfOpen(_ pid: pid_t) {
    if let down = CGEvent(keyboardEventSource: nil, virtualKey: 53, keyDown: true),
       let up = CGEvent(keyboardEventSource: nil, virtualKey: 53, keyDown: false) {
        down.postToPid(pid)
        up.postToPid(pid)
    }
}
func path(_ dir: String, _ request: String) -> String {
    let hash = SHA256.hash(data: Data(request.utf8)).map { String(format: "%02x", $0) }.joined()
    return (dir as NSString).appendingPathComponent("gemini-" + hash + ".json")
}
func safeDir(_ dir: String) -> Bool {
    do { try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700]) } catch { return false }
    var s = stat()
    return lstat(dir, &s) == 0 && (s.st_mode & S_IFMT) == S_IFDIR && s.st_uid == getuid() && (s.st_mode & 0o077) == 0
}
func save(_ b: Binding, _ file: String, create: Bool = false) -> Bool {
    guard let data = try? JSONEncoder().encode(b) else { return false }
    let temp = file + "." + UUID().uuidString + ".tmp"
    let fd = open(temp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
    guard fd >= 0 else { return false }
    let ok = data.withUnsafeBytes { p -> Bool in
        guard let base = p.baseAddress else { return false }
        return write(fd, base, p.count) == p.count && fsync(fd) == 0
    }
    close(fd)
    guard ok else { unlink(temp); return false }
    if create {
        let linked = link(temp,file) == 0
        unlink(temp)
        return linked
    }
    guard rename(temp,file) == 0 else { unlink(temp); return false }
    return true
}
// Revalidate recorded files on every replay. This does not download, create,
// or infer an original from a preview or URL.
func verifiedArtifacts(_ b: Binding, _ dir: String) -> [[String:Any]]? {
    guard let artifacts = b.artifacts, !artifacts.isEmpty, artifacts.count <= 16 else { return nil }
    let root = URL(fileURLWithPath:dir,isDirectory:true).appendingPathComponent("artifacts",isDirectory:true).standardizedFileURL.path
    var directory = stat()
    guard lstat(root,&directory) == 0, (directory.st_mode & S_IFMT) == S_IFDIR,
          directory.st_uid == getuid(), (directory.st_mode & 0o077) == 0 else { return nil }
    var total: Int64 = 0, seen = Set<String>(), verified: [[String:Any]] = []
    for a in artifacts {
        let fileURL = URL(fileURLWithPath:a.path).standardizedFileURL
        guard fileURL.path == a.path, fileURL.deletingLastPathComponent().path == root,
              !a.artifactID.isEmpty, seen.insert(a.artifactID).inserted,
              ["image","video"].contains(a.kind), a.kind == b.capability,
              a.requestID == b.requestID, a.sessionID == b.sessionID,
              a.byteLength > 0, a.byteLength <= 536_870_912,
              a.sha256.count == 64, a.sha256.allSatisfy({ "0123456789abcdef".contains($0) }),
              ((a.kind == "image" && ["image/png","image/jpeg"].contains(a.mimeType)) ||
               (a.kind == "video" && a.mimeType == "video/mp4")) else { return nil }
        total += a.byteLength
        guard total <= 536_870_912 else { return nil }
        let fd = open(a.path,O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { return nil }
        var st = stat(), after = stat(), link = stat()
        guard fstat(fd,&st) == 0, (st.st_mode & S_IFMT) == S_IFREG, st.st_uid == getuid(),
              (st.st_mode & 0o077) == 0, st.st_size == a.byteLength else { close(fd); return nil }
        let handle = FileHandle(fileDescriptor:fd,closeOnDealloc:false)
        var hasher = SHA256(), magic = Data()
        do {
            while let chunk = try handle.read(upToCount:1_048_576), !chunk.isEmpty {
                if magic.count < 12 { magic.append(chunk.prefix(12-magic.count)) }
                hasher.update(data:chunk)
            }
        } catch { close(fd); return nil }
        let digest = hasher.finalize().map { String(format:"%02x",$0) }.joined()
        guard fstat(fd,&after) == 0, lstat(a.path,&link) == 0,
              st.st_dev == after.st_dev, st.st_ino == after.st_ino, st.st_size == after.st_size,
              st.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec,
              st.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
              st.st_dev == link.st_dev, st.st_ino == link.st_ino else { close(fd); return nil }
        let bytes = [UInt8](magic)
        let magicOK: Bool
        if a.mimeType == "image/png" { magicOK = bytes.starts(with:[0x89,0x50,0x4e,0x47,0x0d,0x0a,0x1a,0x0a]) }
        else if a.mimeType == "image/jpeg" { magicOK = bytes.starts(with:[0xff,0xd8,0xff]) }
        else { magicOK = bytes.count >= 12 && Array(bytes[4..<8]) == [0x66,0x74,0x79,0x70] }
        guard magicOK, digest == a.sha256 else { close(fd); return nil }
        if a.kind == "video" {
            let asset = AVURLAsset(url:fileURL)
            guard !asset.tracks(withMediaType:.video).isEmpty, asset.duration.isNumeric, asset.duration.seconds > 0 else { close(fd); return nil }
        }
        if a.kind == "image" {
            guard let source = CGImageSourceCreateWithURL(fileURL as CFURL,nil),
                  CGImageSourceGetCount(source) > 0,
                  let properties = CGImageSourceCopyPropertiesAtIndex(source,0,nil) as? [CFString:Any],
                  let width = properties[kCGImagePropertyPixelWidth] as? Int, width > 0, width <= 16384,
                  let height = properties[kCGImagePropertyPixelHeight] as? Int, height > 0, height <= 16384,
                  width * height <= 100_000_000,
                  CGImageSourceCreateImageAtIndex(source,0,nil) != nil else { close(fd); return nil }
        }
        var finalFD = stat(), finalLink = stat()
        guard fstat(fd,&finalFD) == 0, lstat(a.path,&finalLink) == 0,
              st.st_dev == finalFD.st_dev, st.st_ino == finalFD.st_ino, st.st_size == finalFD.st_size,
              st.st_mtimespec.tv_sec == finalFD.st_mtimespec.tv_sec,
              st.st_mtimespec.tv_nsec == finalFD.st_mtimespec.tv_nsec,
              st.st_dev == finalLink.st_dev, st.st_ino == finalLink.st_ino else { close(fd); return nil }
        close(fd)
        verified.append(a.wire())
    }
    return verified
}
func load(_ file: String) -> Binding? {
    let fd = open(file, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return nil }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0, (st.st_mode & S_IFMT) == S_IFREG, st.st_uid == getuid(),
          (st.st_mode & 0o077) == 0, st.st_size > 0, st.st_size < maxText * 6 + 16384 else { return nil }
    let data = FileHandle(fileDescriptor: fd, closeOnDealloc: false).readDataToEndOfFile()
    return try? JSONDecoder().decode(Binding.self, from: data)
}
struct Lease: Codable {
    let stateFile: String
    let requestID: String
    let sessionID: String
}
let leaseFile = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent("Library/Caches/GeminiDesktopMCP/gemini-active.json").path
func createLease(_ lease: Lease, at target: String = leaseFile) -> Bool {
    guard let bytes = try? JSONEncoder().encode(lease) else { return false }
    let temp = target + "." + UUID().uuidString + ".tmp"
    let fd = open(temp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
    guard fd >= 0 else { return false }
    let ok = bytes.withUnsafeBytes { p -> Bool in
        guard let base = p.baseAddress else { return false }
        return write(fd, base, p.count) == p.count && fsync(fd) == 0
    }
    close(fd)
    let linked = ok && link(temp,target) == 0
    unlink(temp)
    return linked
}
enum LeaseRelation { case own, other, absent, unknown }
func leaseRelation(_ b: Binding, _ stateFile: String, at target: String = leaseFile) -> LeaseRelation {
    let fd = open(target, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return errno == ENOENT ? .absent : .unknown }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0, (st.st_mode & S_IFMT) == S_IFREG, st.st_uid == getuid(),
          (st.st_mode & 0o077) == 0, st.st_size > 0, st.st_size < 16384 else { return .unknown }
    let data = FileHandle(fileDescriptor: fd, closeOnDealloc: false).readDataToEndOfFile()
    guard let lease = try? JSONDecoder().decode(Lease.self, from: data) else { return .unknown }
    return lease.requestID == b.requestID && lease.sessionID == b.sessionID && lease.stateFile == stateFile ? .own : .other
}
func matchesLease(_ b: Binding, _ stateFile: String, at target: String = leaseFile) -> Bool {
    return leaseRelation(b,stateFile,at:target) == .own
}
func releaseLease(_ b: Binding, _ stateFile: String, at target: String = leaseFile) -> Bool {
    return matchesLease(b,stateFile,at:target) && unlink(target) == 0
}
func progressFilePath(_ dir: String, _ request: String) -> String {
    let hash = SHA256.hash(data: Data(request.utf8)).map { String(format: "%02x", $0) }.joined()
    return (dir as NSString).appendingPathComponent("progress-" + hash + ".json")
}
func writeProgressSnapshot(_ dir: String, _ b: Binding, _ text: String) {
    let target = progressFilePath(dir, b.requestID)
    let payload: [String: Any] = ["request_id": b.requestID, "session_id": b.sessionID, "text": text]
    guard JSONSerialization.isValidJSONObject(payload),
          let data = try? JSONSerialization.data(withJSONObject: payload, options: []) else { return }
    let temp = target + "." + UUID().uuidString + ".tmp"
    let fd = open(temp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
    guard fd >= 0 else { return }
    let ok = data.withUnsafeBytes { p -> Bool in
        guard let base = p.baseAddress else { return false }
        return write(fd, base, p.count) == p.count
    }
    close(fd)
    if ok {
        if rename(temp, target) != 0 { _ = unlink(temp) }
    } else {
        _ = unlink(temp)
    }
}
func clearProgressSnapshot(_ dir: String, _ requestID: String) {
    _ = unlink(progressFilePath(dir, requestID))
}
func reclaimStaleLeaseIfSafe(_ window: AXUIElement?, at target: String = leaseFile) {
    var st = stat()
    guard lstat(target, &st) == 0 else { return }
    let fd = open(target, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return }
    let data = FileHandle(fileDescriptor: fd, closeOnDealloc: false).readDataToEndOfFile()
    close(fd)
    guard let lease = try? JSONDecoder().decode(Lease.self, from: data) else {
        _ = unlink(target)
        return
    }
    guard let old = load(lease.stateFile) else {
        _ = unlink(target)
        return
    }
    if terminalReceipt(old) != nil || nativeProcessGone(old) || windowDefinitelyGone(old) {
        _ = unlink(target)
        return
    }
    let opURL = URL(fileURLWithPath: lease.stateFile)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .appendingPathComponent("operation.json")
    if let opData = try? Data(contentsOf: opURL),
       let opObj = (try? JSONSerialization.jsonObject(with: opData)) as? [String: Any] {
        let opStatus = opObj["status"] as? String ?? ""
        let workerPidNum = opObj["worker_pid"] as? NSNumber
        let workerAlive: Bool = {
            guard let num = workerPidNum, CFGetTypeID(num) != CFBooleanGetTypeID() else { return false }
            let pid = num.int32Value
            guard pid > 1 else { return false }
            return !(kill(pid, 0) == -1 && errno == ESRCH)
        }()
        let activelyGenerating = window.flatMap { quickStatus($0) }?.reason == "generation_in_progress"
        if (!workerAlive && !activelyGenerating) || ["completed", "failed", "cancelled", "not_submitted"].contains(opStatus) {
            _ = unlink(target)
            return
        }
    }
    var stateStat = stat()
    let refMtime = lstat(lease.stateFile, &stateStat) == 0 ? max(st.st_mtimespec.tv_sec, stateStat.st_mtimespec.tv_sec) : st.st_mtimespec.tv_sec
    let ageSec = Int(Date().timeIntervalSince1970) - refMtime
    if ageSec > 15 {
        let activelyGenerating = window.flatMap { quickStatus($0) }?.reason == "generation_in_progress"
        if !activelyGenerating {
            _ = unlink(target)
        }
    }
}
let knownImageAccessError = "Normally I can help with things like this, but I don't seem to have access to that content. You can try again or ask me for something else."
func nativeImageFailure(_ b: Binding, _ ui: UI, _ t: Turn) -> Bool {
    guard b.capability == "image", t.owned, !t.readFailed, !t.tooLarge, t.copy,
          ui.stop == nil, !ui.dialog, ui.draft.isEmpty, !ui.hasAttachment, ui.selectedTool,
          ["描述要创作的图片","Describe your image"].contains(ui.nodes[ui.input].desc),
          t.text == knownImageAccessError else { return false }
    let pendingLabels = ["正在生成图片…","正在生成图片","Generating image…","Generating image...","Creating image…"]
    return !ui.nodes.contains {
        ["AXImage","AXProgressIndicator"].contains($0.role) || pendingLabels.contains($0.desc)
    }
}
struct TerminalReceipt {
    let kind: String
    let reason: String
    let completion: Bool
    let notSubmitted: Bool
    let text: String?
    var failureObserved: Bool = false
}
func terminalReceipt(_ b: Binding) -> TerminalReceipt? {
    guard ["completed","cancelled","not_submitted","terminal_uncertain","failed"].contains(b.phase) else { return nil }
    guard let reason = b.finalReason, let completion = b.finalCompletion else {
        return TerminalReceipt(kind:"uncertain",reason:"terminal_receipt_invalid",completion:false,notSubmitted:false,text:nil)
    }
    // A manifest is terminal only after the owned native save-panel flow.
    let mediaProvenance = b.phase == "completed" && reason == "native_media_export_verified" &&
        ["image","video"].contains(b.capability ?? "") && b.finalExportProvenance == "owned_native_save_panel" &&
        b.mediaExportPhase == "save_sent" && (b.artifacts?.count == 1) &&
        b.artifacts?.first?.path == b.mediaExportPath && b.artifacts?.first?.artifactID == b.mediaExportID
    if (reason == "native_image_export_verified" || reason == "native_media_export_verified" || !(b.artifacts ?? []).isEmpty) && !mediaProvenance {
        return TerminalReceipt(kind:"uncertain",reason:"native_download_provenance_unverified",completion:false,notSubmitted:false,text:nil)
    }
    let chatCompleted = b.phase == "completed" && (b.capability ?? "chat") == "chat" &&
      reason == "owned_response_action_observed" && completion && !(b.finalText ?? "").isEmpty
    let canvasCompleted = b.phase == "completed" && b.capability == "canvas" && reason == "canvas_document_text" &&
      completion && b.canvasBlankVerified == true && b.canvasOpenAttempted == true && b.canvasOpenPerformed == true &&
      (b.priorCanvasTextSHA == nil || b.canvasCloseVerified == true) &&
      !(b.finalCanvasTitle ?? "").isEmpty && !(b.finalText ?? "").isEmpty &&
      b.finalCanvasTextSHA == sha256Text(b.finalText ?? "") &&
      b.finalCanvasLocalID == canvasLocalID(b.finalCanvasTitle ?? "",b.finalText ?? "")
    let researchCompleted = b.phase == "completed" && b.capability == "deep_research" && reason == "research_report_text" &&
      completion && (b.confirmationAttemptedVersion != nil || b.priorResearchApprovalVersion != nil) && b.confirmationPerformed == true &&
      b.canvasBlankVerified == true && b.canvasOpenAttempted == true && b.canvasOpenPerformed == true &&
      (b.priorCanvasTextSHA == nil || b.canvasCloseVerified == true) &&
      !(b.finalCanvasTitle ?? "").isEmpty && !(b.finalText ?? "").isEmpty &&
      b.finalCanvasTextSHA == sha256Text(b.finalText ?? "") &&
      b.finalCanvasLocalID == canvasLocalID(b.finalCanvasTitle ?? "",b.finalText ?? "")
    let completed = chatCompleted || canvasCompleted || researchCompleted || (mediaProvenance && completion)
    let cancelled = b.phase == "cancelled" && reason == "cancelled" && !completion && b.finalText == nil
    let unsent = b.phase == "not_submitted" && !reason.isEmpty && !completion && b.finalText == nil
    let uncertain = b.phase == "terminal_uncertain" &&
      ["cancel_outcome_unknown","native_process_gone_outcome_unknown","response_too_large"].contains(reason) &&
      !completion && b.finalText == nil
    let failed = b.phase == "failed" && b.capability == "image" && reason == "native_error_response" &&
      b.finalFailure == true && !completion && b.finalText == knownImageAccessError && (b.artifacts ?? []).isEmpty
    guard completed || cancelled || unsent || uncertain || failed else {
        return TerminalReceipt(kind:"uncertain",reason:"terminal_receipt_invalid",completion:false,notSubmitted:false,text:nil)
    }
    return TerminalReceipt(kind:failed ? "failed" : (unsent ? "error" : (uncertain ? "uncertain" : "completed")),
                           reason:reason,completion:completion,notSubmitted:unsent,text:b.finalText,failureObserved:failed)
}
// Caller holds the global lock. Reject unverifiable receipts before touching
// the lease; `at` permits isolated tests without touching the device lease.
func reconcileTerminalLease(_ b: Binding, _ file: String, _ result: TerminalReceipt,
                            at target: String = leaseFile) -> String? {
    if ["terminal_receipt_invalid","native_download_provenance_unverified"].contains(result.reason) {
        return result.reason
    }
    if result.notSubmitted && leaseRelation(b,file,at:target) == .unknown { return "terminal_lease_unknown" }
    if matchesLease(b,file,at:target) && !releaseLease(b,file,at:target) { return "terminal_lease_release_failed" }
    return nil
}
// Caller holds the global lock. Return only for non-terminal states.
func emitTerminalIfPresent(_ b: Binding, _ file: String, _ fd: Int32, _ req: [String:Any]) {
    guard let result = terminalReceipt(b) else { return }
    var wireArtifacts: [[String:Any]]?
    if b.finalReason == "native_media_export_verified" && result.completion {
        let dir = URL(fileURLWithPath:file).deletingLastPathComponent().path
        guard let checked = verifiedArtifacts(b,dir), !checked.isEmpty else {
            unlock(fd); event("uncertain","artifact_integrity_failed",req); exit(1)
        }
        wireArtifacts = checked
    }
    if let reason = reconcileTerminalLease(b,file,result) {
        unlock(fd); event("uncertain",reason,req); exit(1)
    }
    unlock(fd)
    let evidence = ["canvas_document_text","research_report_text"].contains(b.finalReason ?? "") && result.completion ? canvasEvidence(b) : nil
    event(result.kind,result.reason,req,result.text,completed:result.completion,
          notSubmitted:result.notSubmitted,failureObserved:result.failureObserved,completionEvidence:evidence,artifacts:wireArtifacts)
    exit(result.kind == "completed" ? 0 : 1)
}
func mayMarkAccepted(_ phase: String) -> Bool { ["submit_pending","submitted"].contains(phase) }
func hasForeignUserHeading(_ ui: UI, expected: [String]) -> Bool {
    let ns = ui.nodes
    let user = ns.indices.filter { isUserHeading(ns, $0) }
    if user.count > expected.count { return true }
    guard !user.isEmpty else { return false }
    var sharedParent: Int? = nil
    var siblings: [Int] = []
    var ordered: [(Int, Int)] = []
    for heading in user {
        guard let userRow = ancestor(ns, heading, role: "AXRow"), ns[userRow].parent >= 0 else { return true }
        let p = ns[userRow].parent
        if let existing = sharedParent {
            guard existing == p else { return true }
        } else {
            sharedParent = p
            siblings = ns.indices.filter { ns[$0].parent == p && ns[$0].role == "AXRow" }
        }
        guard let offset = siblings.firstIndex(of: userRow) else { return true }
        ordered.append((offset, heading))
    }
    ordered.sort { $0.0 < $1.0 }
    let slice = ordered.count == expected.count ? expected : Array(expected.suffix(ordered.count))
    return !zip(ordered, slice).allSatisfy { pair, mark in
        headingMatchesExpected(ns[pair.1].desc, mark)
    }
}
enum TurnDecision: Equatable { case waiting, unowned, readFailed, tooLarge, completed, progress, pending }
func mayFinalizeBeforeStop(_ outcome: TurnDecision, _ phase: String) -> Bool {
    return (outcome == .completed || outcome == .tooLarge) && !["cancel_pending","cancel_sent"].contains(phase)
}
func decision(_ t: Turn, stop: Bool) -> TurnDecision {
    if t.waiting { return .waiting }
    if !t.owned { return .unowned }
    if t.readFailed { return .readFailed }
    if t.tooLarge && t.copy && !stop { return .tooLarge }
    if t.text != nil && t.copy && !stop { return .completed }
    return t.text != nil ? .progress : .pending
}
func definitelyUnsent(_ phase: String) -> Bool { ["prepared","mode_select_pending","mode_selected","model_select_pending","model_selected","input_pending"].contains(phase) }
enum CancelDecision: Equatable { case inactive, retry, requiresCancel, wait, confirmed, unknown }
func cancelDecision(_ phase: String, stop: Bool, explicitCancel: Bool) -> CancelDecision {
    switch (phase,stop) {
    case ("cancel_pending",true): return explicitCancel ? .retry : .requiresCancel
    case ("cancel_pending",false): return .unknown
    case ("cancel_sent",true): return .wait
    case ("cancel_sent",false): return .confirmed
    default: return .inactive
    }
}
func processGone(_ missing: Bool, _ terminated: Bool, _ launch: Double?, _ expected: Double) -> Bool {
    if missing || terminated { return true }
    guard let launch = launch else { return false }
    return abs(launch-expected) >= 0.01
}
func nativeProcessGone(_ b: Binding) -> Bool {
    let missing = kill(b.pid,0) == -1 && errno == ESRCH
    let original = NSRunningApplication(processIdentifier:b.pid)
    return processGone(missing, original?.isTerminated ?? false,
                       original?.launchDate?.timeIntervalSince1970, b.launch)
}
func boundWindow(_ b: Binding) -> (NSRunningApplication, AXUIElement)? {
    guard let a = app(), a.processIdentifier == b.pid,
          abs((a.launchDate?.timeIntervalSince1970 ?? 0) - b.launch) < 0.01,
          let ws = windows(a) else { return nil }
    let matched = ws.filter { windowID($0, a.processIdentifier) == b.window }
    guard matched.count == 1 else { return nil }
    return (a,matched[0])
}
func windowDefinitelyGone(_ b: Binding) -> Bool {
    if b.window == (UInt32(bitPattern: b.pid) &+ 0x4000_0000) {
        guard let a = NSRunningApplication(processIdentifier: b.pid), let ws = windows(a) else { return true }
        return !ws.contains { isGeminiChatWindow($0) }
    }
    guard let list = CGWindowListCopyWindowInfo([.optionAll,.excludeDesktopElements],kCGNullWindowID)
            as? [[String:Any]] else { return false }
    return !list.contains {
        ($0[kCGWindowOwnerPID as String] as? Int32) == b.pid &&
        ($0[kCGWindowNumber as String] as? UInt32) == b.window
    }
}
func bound(_ b: Binding) -> (NSRunningApplication, AXUIElement, UI)? {
    guard let (a,w) = boundWindow(b), let ui = snapshot(w) else { return nil }
    return (a,w,ui)
}
func hasDialog(_ w: AXUIElement) -> Bool {
    var pending = [w], n = 0
    let end = min(operationDeadline,Date().addingTimeInterval(2))
    while let e = pending.popLast(), n < 1200, Date() < end {
        n += 1; AXUIElementSetMessagingTimeout(e,0.1)
        if ["AXDialog","AXSheet"].contains(str(e,kAXRoleAttribute)) { return true }
        if let cs = children(e) { pending.append(contentsOf:cs.reversed()) }
    }
    return false
}
func abortBeforeSubmit(_ b: inout Binding, _ file: String, _ fd: Int32,
                       _ out: [String:Any], _ reason: String) -> Never {
    b.phase = "not_submitted"; b.finalReason = reason; b.finalCompletion = false
    guard save(b,file), releaseLease(b,file) else {
        unlock(fd); event("uncertain","pre_submit_state_not_reconciled",out); exit(1)
    }
    unlock(fd); event("error",reason,out,notSubmitted:true); exit(1)
}
var changed = false
let observerCallback: AXObserverCallback = { _, _, _, _ in changed = true }
final class Watcher {
    var observer: AXObserver?
    var registrations: [(AXUIElement, CFString)] = []
    init(_ pid: pid_t, _ w: AXUIElement) {
        guard AXObserverCreate(pid, observerCallback, &observer) == .success, let o = observer else { return }
        CFRunLoopAddSource(CFRunLoopGetCurrent(), AXObserverGetRunLoopSource(o), .defaultMode)
        let root = AXUIElementCreateApplication(pid)
        for target in [root,w] {
            for name in ["AXValueChanged","AXLayoutChanged","AXCreated","AXUIElementDestroyed","AXWindowCreated","AXTitleChanged"] {
                let n = name as CFString
                if AXObserverAddNotification(o, target, n, nil) == .success { registrations.append((target,n)) }
            }
        }
    }
    func wait() {
        changed = false
        let start = Date()
        _ = CFRunLoopRunInMode(.defaultMode, 0.35, true)
        let elapsed = Date().timeIntervalSince(start)
        if elapsed < 0.18 {
            usleep(useconds_t((0.18 - elapsed) * 1_000_000))
        }
    }
    deinit {
        if let o = observer {
            for (target,n) in registrations { AXObserverRemoveNotification(o,target,n) }
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), AXObserverGetRunLoopSource(o), .defaultMode)
        }
    }
}
func requestObj(_ req: [String: Any], _ b: Binding) -> [String: Any] {
    var r = req; r["session_id"] = b.sessionID; return r
}
// Fast, fail-closed status walk. A positively observed stop or draft wins
// over unrelated stale AX nodes during a streaming response.
func quickStatus(_ w: AXUIElement) -> (busy: Bool, reason: String)? {
    if let ui = snapshot(w) {
        if ui.stop != nil { return (true,"generation_in_progress") }
        if ui.hasAttachment || ui.selectedTool || !ui.draft.isEmpty { return (true,"draft_present") }
        if ui.dialog { return (true,"dialog_present") }
        return (false,"idle")
    }
    // In observed Gemini 1.138 this branch contains the composer and stop.
    // It is only a positive fast path; changes fall back to the full walk.
    var branch = w
    var knownBranch = true
    for index in [0,0,2] {
        AXUIElementSetMessagingTimeout(branch,0.1)
        guard let cs = children(branch), index < cs.count else { knownBranch = false; break }
        branch = cs[index]
    }
    if knownBranch, let cs = children(branch) {
        for e in cs {
            AXUIElementSetMessagingTimeout(e,0.1)
            guard str(e,kAXRoleAttribute) == "AXButton" else { continue }
            if metadata(e,kAXIdentifierAttribute) == "stop_button" { return (true,"generation_in_progress") }
            if let desc = metadata(e,kAXDescriptionAttribute),
               (["移除工具","Remove tool"].contains(desc) || isDraftControl("AXButton",desc,"",help:metadata(e,kAXHelpAttribute))) {
                return (true,"draft_present")
            }
        }
    }
    var pending = [w], examined = 0, inputCount = 0, uncertain = false
    let end = Date().addingTimeInterval(3)
    while let e = pending.popLast() {
        if Date() >= end || examined >= 2000 { return (true,"ui_state_unavailable") }
        examined += 1
        AXUIElementSetMessagingTimeout(e,0.1)
        let (status, value) = attr(e,kAXRoleAttribute)
        guard status == .success, let role = value as? String else { uncertain = true; continue }
        if role == "AXDialog" || role == "AXSheet" { return (true,"dialog_present") }
        if role == "AXButton" {
            let id = str(e, kAXIdentifierAttribute)
            let description = str(e, kAXDescriptionAttribute)
            if id == "stop_button" || ["停止回答","Stop response"].contains(description) { return (true,"generation_in_progress") }
            if ["移除工具","Remove tool"].contains(description) || isDraftControl(role,description,id,help:str(e,kAXHelpAttribute)) { return (true,"draft_present") }
        } else if role == "AXTextArea" {
            let desc = str(e, kAXDescriptionAttribute)
            if composerNames.contains(desc) {
                inputCount += 1
                let (vstatus,v) = attr(e,kAXValueAttribute)
                guard vstatus == .success, let text = v as? String else { uncertain = true; continue }
                if !text.isEmpty { return (true,"draft_present") }
            }
        }
        guard let cs = children(e) else { if role == "AXWindow" { uncertain = true }; continue }
        pending.append(contentsOf:cs.reversed())
    }
    guard !uncertain && inputCount == 1 else { return (true,"ui_state_unavailable") }
    return (false,"idle")
}
// Package-provided strings are discovery hints, not evidence that a mode is
// visible to this account or that its selector is safe to activate.
func packagedModeHints() -> [String:[String:String]] {
    let keys = ["image":"TOOL_PLACEHOLDER_IMAGE_GENERATION",
                "video":"TOOL_PLACEHOLDER_VIDEO_GENERATION",
                "deep_research":"TOOL_PLACEHOLDER_DEEP_RESEARCH",
                "canvas":"TOOL_PLACEHOLDER_CANVAS"]
    guard let appURL = NSWorkspace.shared.urlForApplication(withBundleIdentifier:bundleID) else { return [:] }
    var result: [String:[String:String]] = [:]
    for locale in ["zh_CN","en"] {
        let url = appURL.appendingPathComponent("Contents/Resources/\(locale).lproj/App.strings")
        guard let data = try? Data(contentsOf:url),
              let root = try? PropertyListSerialization.propertyList(from:data,options:[],format:nil),
              let dict = root as? [String:String] else { continue }
        for (mode,key) in keys {
            if let value = dict[key], !value.isEmpty { result[mode,default:[:]][locale] = value }
        }
    }
    return result
}
struct CandidateEvidence: Equatable {
    let kind: String
    let count: Int
}
func candidateEvidence(_ nodes: [Node], _ answerRow: Int) -> [CandidateEvidence] {
    var counts: [String:Int] = [:]
    for index in nodes.indices where inside(nodes,index,answerRow) {
        let node = nodes[index]
        var kind: String?
        if ["AXImage","AXButton"].contains(node.role) &&
            ["生成的图片","Generated image"].contains(node.desc) { kind = "generated_image" }
        else if ["AXStaticText","AXGroup"].contains(node.role) &&
            ["正在生成视频…","Generating your video…"].contains(node.desc) { kind = "video_generating" }
        else if node.role == "AXButton" && ["开始研究","Start research"].contains(node.desc) { kind = "research_confirmation" }
        else if node.role == "AXButton" && node.desc == "Canvas" { kind = "canvas" }
        if let kind = kind { counts[kind,default:0] += 1 }
    }
    return counts.keys.sorted().map { CandidateEvidence(kind:$0,count:counts[$0]!) }
}
func ownedAnswerRow(_ ui: UI, _ marker: String, _ isOwned: Bool) -> Int? {
    guard isOwned else { return nil }
    let ns = ui.nodes
    let allHeadings = ns.indices.filter { isUserHeading(ns, $0) }
    var sharedParent: Int? = nil
    var rows: [Int] = []
    var ordered: [(Int, Int)] = []
    for heading in allHeadings {
        guard let row = ancestor(ns, heading, role: "AXRow"), ns[row].parent >= 0 else { continue }
        let p = ns[row].parent
        if let existing = sharedParent {
            guard existing == p else { continue }
        } else {
            sharedParent = p
            rows = ns.indices.filter { ns[$0].parent == p && ns[$0].role == "AXRow" }
        }
        if let offset = rows.firstIndex(of: row) {
            ordered.append((offset, heading))
        }
    }
    ordered.sort { $0.0 < $1.0 }
    guard let last = ordered.last, headingMatchesExpected(ns[last.1].desc, marker) else {
        return nil
    }
    let answerOffset = last.0 + 1
    guard answerOffset < rows.count else { return nil }
    return rows[answerOffset]
}
func sha256Text(_ text: String) -> String {
    return SHA256.hash(data:Data(text.utf8)).map { String(format:"%02x",$0) }.joined()
}
func canvasLocalID(_ title: String, _ text: String) -> String {
    guard let bytes = try? JSONEncoder().encode([title,text]) else { return "" }
    return "local-canvas-sha256:" + SHA256.hash(data:bytes).map { String(format:"%02x",$0) }.joined()
}
func canvasEvidence(_ b: Binding) -> [String:Any]? {
    guard let digest = b.finalCanvasTextSHA, let title = b.finalCanvasTitle, !title.isEmpty,
          let text = b.finalText, digest == sha256Text(text),
          b.finalCanvasLocalID == canvasLocalID(title,text) else { return nil }
    let kind = b.capability == "deep_research" ? "research_report_text" : "canvas_document_text"
    return ["kind":kind,"request_id":b.requestID,"session_id":b.sessionID,"text_sha256":digest]
}
func nodeFrame(_ e: AXUIElement) -> CGRect? {
    let (ps,p) = attr(e,kAXPositionAttribute), (ss,s) = attr(e,kAXSizeAttribute)
    guard ps == .success, ss == .success, let p = p, let s = s,
          CFGetTypeID(p) == AXValueGetTypeID(), CFGetTypeID(s) == AXValueGetTypeID() else { return nil }
    var origin = CGPoint.zero, size = CGSize.zero
    guard AXValueGetValue(p as! AXValue,.cgPoint,&origin), AXValueGetValue(s as! AXValue,.cgSize,&size),
          origin.x.isFinite, origin.y.isFinite, size.width.isFinite, size.height.isFinite,
          size.width > 0, size.height > 0 else { return nil }
    return CGRect(origin:origin,size:size)
}
// Uses the observed Canvas card shape in the latest owned answer: the label,
// title and timestamp are siblings, while an AXUnknown inline card contains
// the title frame. Coordinates are never accepted from a request.
func canvasCard(_ b: Binding, _ ui: UI, allowDraft: Bool = false) -> (Int,String,CGPoint)? {
    let t = turn(ui,marker:b.matchTarget,history:b.matchHistory,allowDraft:allowDraft)
    guard ["canvas","deep_research"].contains(b.capability ?? ""),
          (b.capability != "deep_research" || b.confirmationPerformed == true),
          t.owned, t.copy, !t.readFailed, !t.waiting,
          ui.stop == nil, (allowDraft || ui.draft.isEmpty), !ui.hasAttachment, !ui.dialog,
          let row = ownedAnswerRow(ui,b.matchTarget,true) else { return nil }
    let ns = ui.nodes, indices = ns.indices.filter { inside(ns,$0,row) }
    var values: [Int:String] = [:]
    for i in indices where ns[i].role == "AXStaticText" {
        let (status,value) = attr(ns[i].element,kAXValueAttribute)
        guard status == .success, let s = value as? String, s.utf8.count <= 512 else { return nil }
        values[i] = s
    }
    if b.capability == "deep_research" {
        let pending = ["Researching…","正在研究…","Researching websites…","正在研究网站…",
                       "Analyzing results…","正在分析结果…","Creating your report…","正在创建报告…",
                       "Writing your report…","正在撰写报告…"]
        guard !indices.contains(where: { ns[$0].role == "AXProgressIndicator" ||
            (ns[$0].role == "AXButton" && ["开始研究","Start research"].contains(ns[$0].desc)) }),
              !values.values.contains(where: { pending.contains($0) ||
                  ($0.hasPrefix("Researching ") && $0.contains(" website")) ||
                  ($0.hasPrefix("正在研究 ") && $0.contains(" 个网站")) }) else { return nil }
    }
    let cardLabels = b.capability == "deep_research" ? ["Research"] : ["canvas","Canvas"]
    let marks = values.keys.filter { cardLabels.contains(values[$0]!) }
    guard marks.count == 1, let labelFrame = nodeFrame(ns[marks[0]].element) else { return nil }
    let siblings = values.keys.filter { $0 != marks[0] && ns[$0].parent == ns[marks[0]].parent && !(values[$0] ?? "").isEmpty }
    guard siblings.count == 2 else { return nil }
    var matches: [(Int,CGRect,Double)] = []
    for i in siblings {
        guard let r = nodeFrame(ns[i].element) else { return nil }
        if r.minX > labelFrame.minX && abs(r.midY-labelFrame.midY) < labelFrame.height {
            matches.append((i,r,abs(r.midY-labelFrame.midY)))
        }
    }
    matches.sort { $0.2 < $1.2 }
    guard let best = matches.first, (matches.count == 1 || matches[1].2 > best.2 + 1),
          let title = values[best.0], !title.isEmpty else { return nil }
    let point = CGPoint(x:best.1.midX,y:best.1.midY)
    let cards = indices.filter { ns[$0].role == "AXUnknown" && nodeFrame(ns[$0].element)?.contains(point) == true }
    guard cards.count == 1 else { return nil }
    return (best.0,title,point)
}
func canvasSideAreas(_ b: Binding, _ ui: UI) -> [Int]? {
    guard let row = ownedAnswerRow(ui,b.matchTarget,true),
          let chatScroll = ancestor(ui.nodes,row,role:"AXScrollArea"),
          let inputScroll = ancestor(ui.nodes,ui.input,role:"AXScrollArea") else { return nil }
    let ns = ui.nodes, panel = ns[chatScroll].parent
    return ns.indices.filter { i in
        guard ns[i].role == "AXTextArea", ns[i].desc.isEmpty, let scroll = ancestor(ns,i,role:"AXScrollArea") else { return false }
        return scroll != chatScroll && scroll != inputScroll && ns[scroll].parent == panel
    }
}
func canvasCloseControl(_ b: Binding, _ ui: UI) -> Int? {
    guard let areas = canvasSideAreas(b,ui), areas.count == 1,
          let card = canvasCard(b,ui), canvasDocument(b,ui,card.1) != nil else { return nil }
    let ns = ui.nodes
    // A generic Close is never enough. Require a dedicated Canvas identifier
    // or exact accessible help as well as its native close label.
    let matches = ns.indices.filter { i in
        guard ns[i].role == "AXButton", ["Close","关闭"].contains(ns[i].desc) else { return false }
        let idOK = ["close-canvas","canvas-close","OMNICHAT_TOOLBAR_CLOSE_CANVAS"].contains(ns[i].ident)
        let help = metadata(ns[i].element,kAXHelpAttribute)
        return (idOK || ["Close canvas","关闭 Canvas"].contains(help ?? "")) && enabled(ns[i].element)
    }
    return matches.count == 1 ? matches[0] : nil
}
func canvasDocument(_ b: Binding, _ ui: UI, _ title: String, allowDraft: Bool = false) -> String? {
    guard let card = canvasCard(b,ui,allowDraft:allowDraft), card.1 == title,
          let areas = canvasSideAreas(b,ui), areas.count == 1 else { return nil }
    let e = ui.nodes[areas[0]].element
    let (vs,raw) = attr(e,kAXValueAttribute), (cs,count) = attr(e,"AXNumberOfCharacters")
    guard vs == .success, let text = raw as? String, !text.isEmpty, text.utf8.count <= maxText,
          !text.unicodeScalars.contains(where: { $0.value == 0xFFFC }), cs == .success, let n = count as? Int,
          n == (text as NSString).length else { return nil }
    return text
}
// A download must be inside the same uniquely identified generated-media
// container in the latest owned answer, never a generic unrelated download.
func mediaDownloadControl(_ b: Binding, _ ui: UI, _ t: Turn) -> Int? {
    guard ["image","video"].contains(b.capability ?? ""), t.owned, t.copy, !t.readFailed,
          ui.stop == nil, ui.draft.isEmpty, !ui.hasAttachment, !ui.dialog,
          let row = ownedAnswerRow(ui,b.matchTarget,true) else { return nil }
    let ns = ui.nodes, indices = ns.indices.filter { inside(ns,$0,row) }
    let media = indices.filter { i in
        if b.capability == "image" { return ["AXImage","AXButton"].contains(ns[i].role) && ["生成的图片","Generated image"].contains(ns[i].desc) }
        return ["AXVideo","AXGroup"].contains(ns[i].role) && ["生成的视频","Generated video"].contains(ns[i].desc)
    }
    guard media.count == 1 else { return nil }
    let downloadLabels = ["下载","Download","Save Image…","保存图片…","Save Video…","保存视频…"]
    let downloads = indices.filter { ns[$0].role == "AXButton" && downloadLabels.contains(ns[$0].desc) }
    var scoped: [Int] = [], parent = ns[media[0]].parent
    while parent >= 0 && inside(ns,parent,row) {
        let matches = downloads.filter { inside(ns,$0,parent) }
        if !matches.isEmpty { scoped = matches; break }
        parent = ns[parent].parent
    }
    guard scoped.count == 1, enabled(ns[scoped[0]].element) else { return nil }
    return scoped[0]
}
// A save sheet is accepted only when attached to the bound original window,
// with a unique labeled filename field and Save button.
func mediaSaveSheet(_ b: Binding, requireEnabled: Bool = true) -> (AXUIElement,AXUIElement,AXUIElement)? {
    guard let (a,w) = boundWindow(b), NSWorkspace.shared.frontmostApplication?.processIdentifier == b.pid,
          let focused = focusedWindow(a), CFEqual(focused,w) else { return nil }
    let (status,value) = attr(w,"AXSheets")
    guard status == .success, let sheets = value as? [AXUIElement], sheets.count == 1 else { return nil }
    let sheet = sheets[0]; var pid: pid_t = 0
    guard AXUIElementGetPid(sheet,&pid) == .success, pid == b.pid else { return nil }
    var queue = [sheet], index = 0, fields: [AXUIElement] = [], buttons: [AXUIElement] = []
    while index < queue.count && index < 256 {
        let e = queue[index]; index += 1
        let (rs,rv) = attr(e,kAXRoleAttribute)
        guard rs == .success, let role = rv as? String,
              let desc = metadata(e,kAXDescriptionAttribute), let title = metadata(e,kAXTitleAttribute) else { return nil }
        if role == "AXTextField" && ["Save As:","Save As","存储为：","存储为:","保存为：","保存为:"].contains(desc.isEmpty ? title : desc) { fields.append(e) }
        if role == "AXButton" && ["Save","存储","保存"].contains(desc.isEmpty ? title : desc) { buttons.append(e) }
        guard let cs = children(e) else { return nil }; queue.append(contentsOf:cs)
    }
    guard index == queue.count, fields.count == 1, buttons.count == 1,
          !requireEnabled || enabled(buttons[0]) else { return nil }
    return (sheet,fields[0],buttons[0])
}
func mediaSheetDirectory(_ sheet: AXUIElement) -> String? {
    for key in ["AXURL","AXDocument"] {
        let (status,value) = attr(sheet,key)
        guard status == .success || status == .attributeUnsupported || status == .noValue else { return nil }
        if let url = value as? URL, url.isFileURL { return url.standardizedFileURL.path }
        if let s = value as? String {
            if let url = URL(string:s), url.isFileURL { return url.standardizedFileURL.path }
            if s.hasPrefix("/") { return URL(fileURLWithPath:s).standardizedFileURL.path }
        }
    }
    return nil
}
// A transient metadata failure is not proof that a nested dialog disappeared.
func mediaNestedDialogCount(_ sheet: AXUIElement) -> Int? {
    var queue = [sheet], index = 0, count = 0
    while index < queue.count && index < 256 {
        let e = queue[index]; index += 1
        let (status,value) = attr(e,kAXRoleAttribute)
        guard status == .success, let role = value as? String else { return nil }
        if index > 1 && ["AXSheet","AXDialog"].contains(role) { count += 1; continue }
        guard let cs = children(e) else { return nil }; queue.append(contentsOf:cs)
    }
    return index == queue.count ? count : nil
}
// Only a labeled Go-to-Folder child of the owned native Save sheet qualifies.
func mediaGoFolder(_ b: Binding) -> (AXUIElement,AXUIElement,AXUIElement)? {
    guard let (saveSheet,_,_) = mediaSaveSheet(b,requireEnabled:false) else { return nil }
    var queue = [saveSheet], index = 0, dialogs: [AXUIElement] = []
    while index < queue.count && index < 256 {
        let e = queue[index]; index += 1
        if index > 1 {
            let (rs,rv) = attr(e,kAXRoleAttribute)
            guard rs == .success, let role = rv as? String else { return nil }
            if ["AXSheet","AXDialog"].contains(role) { dialogs.append(e); continue }
        }
        guard let cs = children(e) else { return nil }; queue.append(contentsOf:cs)
    }
    guard index == queue.count, dialogs.count == 1 else { return nil }
    let dialog = dialogs[0]
    var nodes = [dialog], pos = 0, fields: [AXUIElement] = [], buttons: [AXUIElement] = [], labeled = false
    let headings = Set(["Go to Folder","Go to the folder:","Go to the folder","前往文件夹","前往文件夹：","前往文件夹:"])
    while pos < nodes.count && pos < 128 {
        let e = nodes[pos]; pos += 1
        let (rs,rv) = attr(e,kAXRoleAttribute)
        guard rs == .success, let role = rv as? String,
              let desc = metadata(e,kAXDescriptionAttribute), let title = metadata(e,kAXTitleAttribute) else { return nil }
        if headings.contains(desc) || headings.contains(title) { labeled = true }
        if role == "AXStaticText", let value = attr(e,kAXValueAttribute).1 as? String, headings.contains(value) { labeled = true }
        if role == "AXTextField" { fields.append(e) }
        if role == "AXButton" && ["Go","前往"].contains(desc.isEmpty ? title : desc) { buttons.append(e) }
        guard let cs = children(e) else { return nil }; nodes.append(contentsOf:cs)
    }
    guard pos == nodes.count, labeled, fields.count == 1, buttons.count == 1, enabled(buttons[0]) else { return nil }
    return (dialog,fields[0],buttons[0])
}
func mediaArtifact(_ b: Binding, _ dir: String) -> ImageArtifact? {
    guard b.mediaExportPhase == "save_sent", let path = b.mediaExportPath, let id = b.mediaExportID,
          let kind = b.capability, ["image","video"].contains(kind) else { return nil }
    let root = URL(fileURLWithPath:dir).appendingPathComponent("artifacts",isDirectory:true).standardizedFileURL.path
    guard URL(fileURLWithPath:path).standardizedFileURL.path == path,
          URL(fileURLWithPath:path).deletingLastPathComponent().path == root, safeDir(root) else { return nil }
    let fd = open(path,O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return nil }; defer { close(fd) }
    var before = stat(), after = stat(), link = stat()
    guard fstat(fd,&before) == 0, (before.st_mode & S_IFMT) == S_IFREG, before.st_uid == getuid(),
          before.st_size > 0, before.st_size <= 536_870_912, fchmod(fd,0o600) == 0 else { return nil }
    let handle = FileHandle(fileDescriptor:fd,closeOnDealloc:false); var hash = SHA256()
    do { while let chunk = try handle.read(upToCount:1_048_576), !chunk.isEmpty { hash.update(data:chunk) } } catch { return nil }
    guard fstat(fd,&after) == 0, lstat(path,&link) == 0,
          before.st_dev == after.st_dev, before.st_ino == after.st_ino, before.st_size == after.st_size,
          before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec, before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
          before.st_dev == link.st_dev, before.st_ino == link.st_ino else { return nil }
    return ImageArtifact(artifactID:id,kind:kind,mimeType:kind == "image" ? "image/png" : "video/mp4",path:path,
        byteLength:before.st_size,sha256:hash.finalize().map { String(format:"%02x",$0) }.joined(),
        requestID:b.requestID,sessionID:b.sessionID)
}
// Confirm only the exact ordinary research-start control in the latest owned
// answer. The version binds the whole rendered answer and must be freshly
// rechecked immediately before an at-most-once action.
func researchStartAction(_ b: Binding, _ ui: UI, _ t: Turn,
                         valueReader: ((Int) -> String?)? = nil,
                         enabledReader: ((Int) -> Bool)? = nil) -> (Int,String)? {
    guard b.capability == "deep_research", t.owned, !t.waiting, !t.readFailed,
          ui.stop == nil, ui.draft.isEmpty, !ui.hasAttachment, !ui.dialog,
          let row = ownedAnswerRow(ui,b.matchTarget,true) else { return nil }
    let ns = ui.nodes, indices = ns.indices.filter { inside(ns,$0,row) }
    let buttons = indices.filter { ns[$0].role == "AXButton" &&
        ["开始研究","Start research"].contains(ns[$0].desc) }
    guard buttons.count == 1,
          enabledReader?(buttons[0]) ?? enabled(ns[buttons[0]].element) else { return nil }
    var material: [[String]] = []
    for i in indices {
        let n = ns[i]; var value = ""
        if ["AXStaticText","AXTextArea","AXHeading"].contains(n.role) {
            let text: String
            if let reader = valueReader {
                guard let v = reader(i) else { return nil }; text = v
            } else {
                let (status,raw) = attr(n.element,kAXValueAttribute)
                guard status == .success, let v = raw as? String else { return nil }; text = v
            }
            guard text.utf8.count <= maxText else { return nil }
            value = text
        }
        material.append([n.role,n.ident,n.desc,value])
    }
    guard let body = try? JSONEncoder().encode(material), body.count <= 1_048_576,
          var bytes = try? JSONEncoder().encode([b.requestID,b.sessionID,b.marker]) else { return nil }
    bytes.append(body)
    let digest = SHA256.hash(data:bytes).map { String(format:"%02x",$0) }.joined()
    return (buttons[0],digest)
}
func researchPending(_ version: String) -> [String:Any] {
    return ["action_id":"deep_research_start","action_version":version,
            "kind":"research_start","scope":"owned_research_plan"]
}
func toolConsentAction(_ b: Binding, _ ui: UI, _ t: Turn) -> (allowIdx: Int, denyIdx: Int, version: String, summary: String)? {
    guard t.owned, t.waiting, !t.readFailed, !ui.dialog,
          let row = ownedAnswerRow(ui, b.matchTarget, true) else { return nil }
    let ns = ui.nodes
    let indices = ns.indices.filter { inside(ns, $0, row) }
    let yesLabels = Set(["允许在此对话中使用", "Allow for this chat", "允许一次", "Allow once", "允许", "Allow"])
    let noLabels = Set(["不允许", "Don\u{2019}t allow", "Don't allow", "拒绝", "Deny", "以后再说", "Not now"])
    let allowCandidates = indices.filter { i in
        ns[i].role == "AXButton" && enabled(ns[i].element) &&
        (ns[i].ident == "tool-consent-allow" || yesLabels.contains(ns[i].desc))
    }
    let denyCandidates = indices.filter { i in
        ns[i].role == "AXButton" && enabled(ns[i].element) &&
        (ns[i].ident == "tool-consent-deny" || noLabels.contains(ns[i].desc))
    }
    guard let allowIdx = allowCandidates.first, let denyIdx = denyCandidates.first else { return nil }
    var texts: [String] = []
    var material: [[String]] = []
    for i in indices {
        let n = ns[i]
        var val = ""
        if ["AXStaticText", "AXTextArea", "AXHeading"].contains(n.role),
           let v = attr(n.element, kAXValueAttribute).1 as? String {
            let trimmed = v.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty && trimmed.utf8.count <= 2048 {
                val = trimmed
                if texts.count < 6 { texts.append(trimmed) }
            }
        }
        material.append([n.role, n.ident, n.desc, val])
    }
    guard let body = try? JSONEncoder().encode(material),
          var bytes = try? JSONEncoder().encode([b.requestID, b.sessionID, b.marker]) else { return nil }
    bytes.append(body)
    let digest = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
    let summary = texts.isEmpty ? "Gemini.app 请求工具执行权限" : texts.joined(separator: " · ")
    return (allowIdx, denyIdx, digest, String(summary.prefix(240)))
}
func toolConsentPending(_ version: String, _ summary: String) -> [String:Any] {
    return ["action_id":"tool_consent_allow","action_version":version,
            "kind":"tool_consent","scope":"owned_tool_consent","summary":summary]
}
// Return one exact native usage section, or fail closed on unknown structure.
func usageSection(_ w: AXUIElement) -> AXUIElement? {
    var pending = [w], index = 0, sections: [AXUIElement] = []
    let end = Date().addingTimeInterval(5)
    while index < pending.count && index < 1200 && Date() < end {
        let e = pending[index]; index += 1; AXUIElementSetMessagingTimeout(e,0.1)
        let (rs,rv) = attr(e,kAXRoleAttribute)
        guard rs == .success, let role = rv as? String else { return nil }
        if ["AXGroup","AXWindow","AXScrollArea"].contains(role) {
            let (ids,idv) = attr(e,kAXIdentifierAttribute)
            if ids == .success && (idv as? String) == "SETTINGS_ELEMENT_ID_USAGE_LIMITS" { sections.append(e); continue }
            if ![.success,.attributeUnsupported,.noValue].contains(ids) { return nil }
        }
        guard let cs = children(e) else { return nil }; pending.append(contentsOf:cs)
    }
    return index == pending.count && sections.count == 1 ? sections[0] : nil
}
func focusedWindow(_ a: NSRunningApplication) -> AXUIElement? {
    let (status,value) = attr(AXUIElementCreateApplication(a.processIdentifier),kAXFocusedWindowAttribute)
    guard status == .success, let value = value, CFGetTypeID(value) == AXUIElementGetTypeID() else { return nil }
    return (value as! AXUIElement)
}
func uniqueControl(_ root: AXUIElement, roles: Set<String>, labels: Set<String>) -> AXUIElement? {
    var queue = [root], index = 0, matches: [AXUIElement] = []
    while index < queue.count && index < 1200 {
        let e = queue[index]; index += 1
        let (rs,rv) = attr(e,kAXRoleAttribute)
        guard rs == .success, let role = rv as? String else { return nil }
        if roles.contains(role) {
            guard let desc = metadata(e,kAXDescriptionAttribute), let title = metadata(e,kAXTitleAttribute) else { return nil }
            if labels.contains(desc) || labels.contains(title) { matches.append(e) }
        }
        guard let cs = children(e) else { return nil }; queue.append(contentsOf:cs)
    }
    return index == queue.count && matches.count == 1 && enabled(matches[0]) ? matches[0] : nil
}
// Only inspect an explicitly identified native usage-limits section in the
// currently focused window. Static localization strings are not quota data.
func usageObservation() -> [String:Any] {
    let timestamp = ISO8601DateFormatter().string(from:Date())
    var result: [String:Any] = ["version":version,"status":"unknown",
        "source":"gemini_desktop_usage_limits_current_view","collected_at":timestamp,
        "scope":"unknown","buckets":[],"remaining_credits":NSNull(),"percent_used":NSNull(),
        "reset_label":NSNull(),"updated_label":NSNull(),"reason":"usage_surface_not_observed"]
    guard AXIsProcessTrusted(), let a = app(), let w = focusedWindow(a), let section = usageSection(w) else { return result }
    let end = Date().addingTimeInterval(5)
    var items: [(AXUIElement,Int)] = [(section,-1)], pos = 0, labels: [(Int,String)] = []
    while pos < items.count && pos < 500 && Date() < end {
        let (e,parent) = items[pos]; let nodeIndex = pos; pos += 1
        let (rs,rv) = attr(e,kAXRoleAttribute)
        guard rs == .success, let role = rv as? String else { return result }
        if ["AXStaticText","AXHeading"].contains(role) {
            let (vs,vv) = attr(e,kAXValueAttribute)
            guard vs == .success, let value = vv as? String, value.utf8.count <= 512 else { return result }
            labels.append((parent,value))
        }
        guard let cs = children(e) else { return result }
        items.append(contentsOf:cs.map { ($0,nodeIndex) })
    }
    guard pos == items.count, labels.contains(where: { ["Usage limits","用量限额"].contains($0.1) }) else { return result }
    func capture(_ pattern: String, _ label: String) -> String? {
        guard let regex = try? NSRegularExpression(pattern:pattern),
              let match = regex.firstMatch(in:label,range:NSRange(label.startIndex...,in:label)),
              match.numberOfRanges == 2, let range = Range(match.range(at:1),in:label) else { return nil }
        return String(label[range])
    }
    var grouped: [Int:[String]] = [:]
    for (parent,label) in labels where parent >= 0 { grouped[parent,default:[]].append(label) }
    var buckets: [[String:Any]] = []
    for parent in grouped.keys.sorted() {
        var credits: [Int64] = [], percentages: [Int] = [], resets: [String] = [], updates: [String] = [], names: [String] = []
        for label in grouped[parent]! {
            if let raw = capture("^([0-9]+) credits remaining$",label) ?? capture("^剩余点数：([0-9]+)$",label), let v = Int64(raw) { credits.append(v); continue }
            if let raw = capture("^([0-9]+)% used$",label) ?? capture("^已使用 ([0-9]+)%$",label), let v = Int(raw), (0...100).contains(v) { percentages.append(v); continue }
            if label.hasPrefix("Resets ") || label.hasPrefix("重置时间：") { resets.append(label); continue }
            if label == "Updated just now" || label == "更新时间：刚刚" || capture("^Updated ([0-9]+) min ago$",label) != nil || capture("^更新时间：([0-9]+) 分钟前$",label) != nil { updates.append(label); continue }
            if !label.isEmpty && !["Usage limits","用量限额","Reload","重新加载","Learn more","了解详情","Retry","重试"].contains(label) { names.append(label) }
        }
        // One direct row name and one metric are required; never merge
        // credits and percentages from unrelated visible rows.
        guard names.count == 1, credits.count + percentages.count == 1,
              resets.count <= 1, updates.count <= 1 else { continue }
        var bucket: [String:Any] = ["row_label":names[0],"scope":"unknown",
            "remaining_credits":NSNull(),"percent_used":NSNull(),"reset_label":NSNull(),"updated_label":NSNull()]
        if let v = credits.first { bucket["remaining_credits"] = v }
        if let v = percentages.first { bucket["percent_used"] = v }
        if let v = resets.first { bucket["reset_label"] = v }
        if let v = updates.first { bucket["updated_label"] = v }
        buckets.append(bucket)
    }
    guard !buckets.isEmpty else { result["reason"] = "usage_row_scope_unverified"; return result }
    result["status"] = "observed_unverified"; result["reason"] = "usage_section_observed"; result["buckets"] = buckets
    if buckets.count == 1 {
        for key in ["remaining_credits","percent_used","reset_label","updated_label"] { result[key] = buckets[0][key] }
    }
    return result
}
// The refresh action never interrupts a generation or a draft, never closes
// a preexisting settings window, and only presses exact native controls.
func usageRefresh() -> [String:Any] {
    func unavailable(_ reason: String) -> [String:Any] {
        var v = usageObservation(); v["status"] = "unknown"; v["reason"] = reason
        for key in ["remaining_credits","percent_used","reset_label","updated_label"] { v[key] = NSNull() }
        v["buckets"] = []; v["refresh_acknowledged"] = false
        v["ui_restored"] = true; v["ui_restore_reason"] = "no_ui_action"
        return v
    }
    guard let fd = lock() else { return unavailable("helper_busy_or_lock_unavailable") }
    defer { unlock(fd) }
    var st = stat()
    guard lstat(leaseFile,&st) != 0 && errno == ENOENT else { return unavailable("active_binding_exists") }
    guard let a = app(), let ws = windows(a), let main = standardWindow(ws),
          let quick = quickStatus(main), !quick.busy else { return unavailable("app_not_idle") }
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier else {
        return unavailable("foreground_required")
    }
    guard let original = focusedWindow(a), let originalID = windowID(original,a.processIdentifier),
          metadata(original,kAXTitleAttribute) != nil else { return unavailable("original_window_unverified") }
    let initialWindows = ws
    let initialIDs = Set(ws.compactMap { windowID($0,a.processIdentifier) })
    let settingsTitles = Set(["Settings","Preferences","设置"])
    var settingsAttempted = false
    var createdSettings: (AXUIElement,UInt32)?
    func identifyCreatedSettings() {
        guard let focused = focusedWindow(a), let title = metadata(focused,kAXTitleAttribute),
              settingsTitles.contains(title), let id = windowID(focused,a.processIdentifier),
              !initialIDs.contains(id), !initialWindows.contains(where: { CFEqual($0,focused) }) else { return }
        createdSettings = (focused,id)
    }
    // The result is captured before cleanup so a UI-restore problem does not
    // erase observed usage values or misrepresent server freshness.
    let captured: [String:Any] = {
        var current = focusedWindow(a)
        if current.flatMap({ usageSection($0) }) == nil {
            let root = AXUIElementCreateApplication(a.processIdentifier)
            let (status,value) = attr(root,kAXMenuBarAttribute)
            guard status == .success, let raw = value, CFGetTypeID(raw) == AXUIElementGetTypeID(),
                  let settings = uniqueControl(raw as! AXUIElement,roles:["AXMenuItem"],
                                               labels:["Settings…","Settings...","Preferences…","Preferences...","设置…","设置..."]),
                  let rechecked = quickStatus(main), !rechecked.busy,
                  NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier else {
                return unavailable("settings_control_unavailable")
            }
            settingsAttempted = true
            let pressed = press(settings)
            _ = CFRunLoopRunInMode(.defaultMode,0.4,false)
            identifyCreatedSettings()
            guard pressed else { return unavailable("settings_action_outcome_unknown") }
            current = focusedWindow(a)
            guard let settingsWindow = current, let title = metadata(settingsWindow,kAXTitleAttribute),
                  settingsTitles.contains(title),
                  let entry = uniqueControl(settingsWindow,roles:["AXButton","AXRadioButton","AXRow"],labels:["Usage limits","用量限额"]),
                  let rechecked = quickStatus(main), !rechecked.busy,
                  NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier,
                  press(entry) else { return unavailable("usage_settings_control_unavailable") }
            _ = CFRunLoopRunInMode(.defaultMode,0.4,false)
            current = focusedWindow(a)
        }
        guard let w = current, let section = usageSection(w),
              let reload = uniqueControl(section,roles:["AXButton"],labels:["Reload","重新加载"]),
              let rechecked = quickStatus(main), !rechecked.busy,
              NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier,
              press(reload) else { return unavailable("usage_reload_control_unavailable") }
        _ = CFRunLoopRunInMode(.defaultMode,0.5,false)
        var value = usageObservation(); value["refresh_acknowledged"] = true
        return value
    }()
    var result = captured
    if !settingsAttempted {
        result["ui_restored"] = true; result["ui_restore_reason"] = "no_settings_navigation"
        return result
    }
    // The app must still be foreground; never steal focus from another app.
    if createdSettings == nil { identifyCreatedSettings() }
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier,
          let live = windows(a) else {
        result["ui_restored"] = false; result["ui_restore_reason"] = "app_or_windows_unavailable"
        return result
    }
    let novelWindows = live.filter { w in !initialWindows.contains(where: { CFEqual($0,w) }) }
    guard novelWindows.allSatisfy({ w in windowID(w,a.processIdentifier).map { !initialIDs.contains($0) } == true }) else {
        result["ui_restored"] = false; result["ui_restore_reason"] = "new_window_identity_unavailable"
        return result
    }
    let newWindows = novelWindows
    if let (created,createdID) = createdSettings {
        let matches = live.filter { windowID($0,a.processIdentifier) == createdID }
        if !matches.isEmpty {
            guard matches.count == 1, CFEqual(matches[0],created), newWindows.count == 1,
                  focusedWindow(a).map({ CFEqual($0,created) }) == true,
                  metadata(created,kAXTitleAttribute).map({ settingsTitles.contains($0) }) == true,
                  let closeValue = attr(created,kAXCloseButtonAttribute).1,
                  CFGetTypeID(closeValue) == AXUIElementGetTypeID(),
                  enabled(closeValue as! AXUIElement),
                  windowID(created,a.processIdentifier) == createdID,
                  NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier,
                  press(closeValue as! AXUIElement) else {
                result["ui_restored"] = false; result["ui_restore_reason"] = "owned_settings_close_unverified"
                return result
            }
            _ = CFRunLoopRunInMode(.defaultMode,0.3,false)
            guard let after = windows(a),
                  !after.contains(where: { CFEqual($0,created) || windowID($0,a.processIdentifier) == createdID }),
                  after.filter({ w in !initialWindows.contains(where: { CFEqual($0,w) }) }).allSatisfy({ windowID($0,a.processIdentifier) != nil }) else {
                result["ui_restored"] = false; result["ui_restore_reason"] = "owned_settings_close_not_observed"
                return result
            }
        } else if !newWindows.isEmpty {
            result["ui_restored"] = false; result["ui_restore_reason"] = "other_new_window_present"
            return result
        }
    } else {
        // A settings view may have reused a preexisting window or opened in
        // place. Without ownership proof, neither close it nor claim restore.
        result["ui_restored"] = false; result["ui_restore_reason"] = "settings_window_ownership_unverified"
        return result
    }
    guard let after = windows(a) else {
        result["ui_restored"] = false; result["ui_restore_reason"] = "original_window_unavailable"; return result
    }
    let originals = after.filter { windowID($0,a.processIdentifier) == originalID }
    guard originals.count == 1, let originalNow = originals.first, CFEqual(original,originalNow),
          NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier else {
        result["ui_restored"] = false; result["ui_restore_reason"] = "original_window_unavailable"
        return result
    }
    if focusedWindow(a).map({ CFEqual($0,originalNow) }) != true {
        guard AXUIElementPerformAction(originalNow,kAXRaiseAction as CFString) == .success else {
            result["ui_restored"] = false; result["ui_restore_reason"] = "original_window_raise_failed"
            return result
        }
        _ = CFRunLoopRunInMode(.defaultMode,0.3,false)
    }
    let restored = focusedWindow(a).map { CFEqual($0,originalNow) } == true
    result["ui_restored"] = restored
    result["ui_restore_reason"] = restored ? "original_window_restored" : "original_focus_not_observed"
    return result
}
// Focus is explicit and only allowed after proving the shared app is idle.
func usageFocus() -> [String:Any] {
    var result: [String:Any] = ["version":version,"status":"unknown","foreground":false,
                                "requires_foreground":true,"reason":"helper_busy_or_lock_unavailable"]
    guard let fd = lock() else { return result }; defer { unlock(fd) }
    var st = stat()
    guard lstat(leaseFile,&st) != 0 && errno == ENOENT else { result["reason"] = "active_binding_exists"; return result }
    guard let a = app(), let ws = windows(a), let main = standardWindow(ws), let quick = quickStatus(main), !quick.busy else {
        result["reason"] = "app_not_idle"; return result
    }
    if NSWorkspace.shared.frontmostApplication?.processIdentifier != a.processIdentifier {
        guard a.activate(options:[.activateIgnoringOtherApps]) else { result["reason"] = "focus_action_failed"; return result }
        _ = CFRunLoopRunInMode(.defaultMode,0.3,false)
    }
    let active = NSWorkspace.shared.frontmostApplication?.processIdentifier == a.processIdentifier
    result["foreground"] = active; result["status"] = active ? "ready" : "unknown"
    result["reason"] = active ? "foreground_observed" : "foreground_not_observed"
    return result
}
// Focus a specifically leased live session without sending or resuming it.
func focusOwned(_ req: [String:Any]) -> [String:Any] {
    var result: [String:Any] = ["version":version,"status":"unknown","foreground":false,
                                "requires_foreground":true,"reason":"invalid_focus_binding"]
    guard let rid = req["request_id"] as? String, !rid.isEmpty, rid.utf8.count <= 256,
          let sid = req["session_id"] as? String, !sid.isEmpty,
          let dir = req["state_dir"] as? String, dir.hasPrefix("/") else { return result }
    var dirInfo = stat()
    guard lstat(dir,&dirInfo) == 0, safeDir(dir) else { return result }
    result["request_id"] = rid; result["session_id"] = sid
    guard let fd = lock() else { result["reason"] = "helper_busy_or_lock_unavailable"; return result }
    defer { unlock(fd) }
    let file = path(dir,rid)
    guard let b = load(file), b.requestID == rid, b.sessionID == sid, matchesLease(b,file),
          ["submit_pending","submitted","accepted","cancel_pending","cancel_sent"].contains(b.phase),
          !nativeProcessGone(b), let (a,w) = boundWindow(b) else { result["reason"] = "focus_binding_not_verified"; return result }
    if NSWorkspace.shared.frontmostApplication?.processIdentifier != b.pid {
        guard a.activate(options:[.activateIgnoringOtherApps]) else { result["reason"] = "focus_action_failed"; return result }
    }
    guard AXUIElementPerformAction(w,kAXRaiseAction as CFString) == .success else {
        result["reason"] = "window_raise_failed"; return result
    }
    _ = CFRunLoopRunInMode(.defaultMode,0.3,false)
    let active = NSWorkspace.shared.frontmostApplication?.processIdentifier == b.pid &&
                 focusedWindow(a).map { CFEqual($0,w) } == true && boundWindow(b) != nil
    result["foreground"] = active; result["status"] = active ? "ready" : "unknown"
    result["reason"] = active ? "bound_window_foreground_observed" : "bound_window_foreground_not_observed"
    return result
}
// BEGIN MAIN
let command = CommandLine.arguments.count == 2 ? CommandLine.arguments[1] : ""
let installed = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) != nil
let trusted = AXIsProcessTrusted()
if command == "usage" { emit(usageObservation()); exit(0) }
if command == "usage_refresh" { emit(usageRefresh()); exit(0) }
if command == "usage_focus" { emit(usageFocus()); exit(0) }
if command == "focus" {
    let data = FileHandle.standardInput.readData(ofLength:1_048_577)
    guard data.count <= 1_048_576 else { emit(["version":version,"status":"unknown","reason":"invalid_focus_binding"]); exit(1) }
    if data.isEmpty { emit(usageFocus()); exit(0) }
    guard let req = (try? JSONSerialization.jsonObject(with:data)) as? [String:Any] else {
        emit(["version":version,"status":"unknown","reason":"invalid_focus_binding"]); exit(1)
    }
    emit(req.isEmpty ? usageFocus() : focusOwned(req)); exit(0)
}
if command == "accessibility" {
    emit([
        "version": version,
        "accessibility_trusted": trusted,
        "app_installed": installed,
    ])
    exit(0)
}
if command == "prompt_accessibility" {
    let promptKey = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
    let opts = [promptKey: true] as CFDictionary
    let nowTrusted = AXIsProcessTrustedWithOptions(opts)
    emit([
        "version": version,
        "accessibility_trusted": nowTrusted,
        "prompted": !nowTrusted,
    ])
    exit(0)
}
if command == "probe" {
    operationDeadline = Date().addingTimeInterval(10)
    let a = app(); let ws = trusted ? a.flatMap { windows($0) } : nil
    let mainWin = ws.flatMap { standardWindow($0) }
    if let fd = lock() {
        reclaimStaleLeaseIfSafe(mainWin)
        unlock(fd)
    }
    let lwProbe = mainWin.map { lightweightNewChatHandle($0) }
    let status = (lwProbe?.ok == true) ? (busy: false, reason: "idle") : mainWin.flatMap { quickStatus($0) }
    var leaseStat = stat()
    let leaseExists = lstat(leaseFile, &leaseStat) == 0 || errno != ENOENT
    let busy = leaseExists || (status?.busy ?? false)
    let ready = installed && trusted
    emit(["version":version,"available":ready,"adapter_ready":ready,
          "accessibility_trusted":trusted,"app_installed":installed,"app_running":a != nil,
          "busy":busy,"reason":leaseExists ? "active_binding_exists" : (status?.reason ?? (a == nil ? "app_not_running" : "window_closed_auto_reopenable"))])
    exit(0)
}
if command == "discover_models" {
    operationDeadline = Date().addingTimeInterval(10)
    let discovered = discoverDesktopModels(forceRefresh: true, installed: installed, trusted: trusted)
    emit(["version": version, "app_installed": installed, "accessibility_trusted": trusted, "discovered_models": discovered])
    exit(0)
}
if command == "capabilities" {
    operationDeadline = Date().addingTimeInterval(8)
    let names = ["chat","image","video","deep_research","canvas"]
    let hints = packagedModeHints()
    let supported = Dictionary(uniqueKeysWithValues:names.map { name in
        (name, ["account_available": NSNull(), "live_verified": false,
                "implementation": name == "chat" ? "implemented_unverified" : (["image","video","deep_research","canvas"].contains(name) ? "experimental_unverified" : "not_implemented"),
                "composer_placeholder_hints": hints[name] ?? [:],
                "hint_source": "packaged_localization",
                "selector_observed": false] as [String:Any])
    })
    let discovered = discoverDesktopModels(forceRefresh: false, installed: installed, trusted: trusted)
    emit(["version":version,"app_installed":installed,"accessibility_trusted":trusted,
          "discovered_models":discovered,
          "capabilities":supported,"requirements":["usage_refresh":["requires_foreground":true],
          "canvas":["requires_foreground_for_card":true],"media_export":["requires_foreground_for_save":true]]])
    exit(0)
}
func parseTimeoutMs(_ raw: Any?) -> Int64? {
    if raw == nil || raw is NSNull { return 0 }
    guard let num = raw as? NSNumber, CFGetTypeID(num) != CFBooleanGetTypeID() else { return nil }
    let intVal = num.int64Value
    guard Double(intVal) == num.doubleValue else { return nil }
    return intVal
}
guard ["run","continue","resume","cancel","inspect","confirm","collect"].contains(command) else { event("error","unknown_command",[:]); exit(1) }
let data = FileHandle.standardInput.readData(ofLength: 1_048_577)
guard data.count <= 1_048_576, let req = (try? JSONSerialization.jsonObject(with:data)) as? [String:Any],
      let rid = req["request_id"] as? String, !rid.isEmpty, rid.utf8.count <= 256,
      let dir = req["state_dir"] as? String, dir.hasPrefix("/"),
      let rawTimeout = parseTimeoutMs(req["timeout_ms"]),
      (rawTimeout == 0 || (rawTimeout >= 100 && rawTimeout <= 86_400_000)) else {
    event("error","invalid_request",[:]); exit(1)
}
operationDeadline = rawTimeout == 0 ? Date.distantFuture : Date().addingTimeInterval(Double(rawTimeout)/1000)
if command == "run" || command == "continue" {
    guard let session = req["session_id"] as? String, UUID(uuidString:session) != nil,
          let prompt = req["prompt"] as? String, !prompt.isEmpty, prompt.utf8.count <= maxText else { event("error","invalid_request",req); exit(1) }
    if let rawModel = req["model"], !(rawModel is NSNull) {
        guard let m = rawModel as? String else { event("error","invalid_request",req); exit(1) }
        guard canonicalDesktopModel(m) != nil else { event("error","unsupported_model_selection",req); exit(1) }
    }
    if let rawEffort = req["effort"], !(rawEffort is NSNull) {
        guard let e = rawEffort as? String else { event("error","invalid_request",req); exit(1) }
        guard canonicalDesktopEffort(e) != nil else { event("error","unsupported_effort_selection",req); exit(1) }
    }
    if let requested = req["capability"] {
        guard let capability = requested as? String else { event("error","invalid_request",req); exit(1) }
        if !["chat","image","video","deep_research","canvas"].contains(capability) { event("error","capability_selection_unverified",req); exit(1) }
    }
    if let inputs = req["inputs"], !(inputs is NSNull) {
        guard let values = inputs as? [Any] else { event("error","invalid_request",req); exit(1) }
        if !values.isEmpty { event("error","attachment_input_unverified",req); exit(1) }
    }
    if let context = req["artifact_context"], !(context is NSNull) {
        event("error","artifact_context_unverified",req); exit(1)
    }
    if command == "continue", (req["prior_request_id"] as? String)?.isEmpty != false {
        event("error","invalid_request",req); exit(1)
    }
} else if (req["session_id"] as? String)?.isEmpty != false { event("error","invalid_request",req); exit(1) }
guard safeDir(dir) else {
    event("error","state_unavailable",req,notSubmitted:["run","continue"].contains(command)); exit(1)
}
guard let fd = lock() else {
    event("error","helper_busy_or_lock_unavailable",req,notSubmitted:["run","continue"].contains(command)); exit(1)
}
let file = path(dir,rid)
if command == "confirm" {
    let requestedActionID = req["action_id"] as? String ?? ""
    guard let b = load(file), b.requestID == rid, b.sessionID == req["session_id"] as? String,
          matchesLease(b,file), b.phase == "accepted",
          ["deep_research_start", "tool_consent_allow", "tool_consent_deny"].contains(requestedActionID),
          let requestedVersion = req["action_version"] as? String, requestedVersion.utf8.count == 64 else {
        unlock(fd); event("uncertain","confirmation_binding_or_action_invalid",req); exit(1)
    }
    guard b.confirmationAttemptedVersion != requestedVersion else {
        unlock(fd); event("uncertain","confirmation_already_attempted_no_replay",req); exit(1)
    }
    if requestedActionID == "deep_research_start" {
        guard b.confirmationAttemptedVersion == nil else {
            unlock(fd); event("uncertain","confirmation_already_attempted_no_replay",req); exit(1)
        }
        guard let (_,w,ui) = bound(b), let candidate = researchStartAction(b,ui,turn(ui,marker:b.matchTarget,history:b.matchHistory)),
              candidate.1 == requestedVersion else {
            unlock(fd); event("uncertain","confirmation_version_or_context_changed",req); exit(1)
        }
        var updated = b; updated.confirmationAttemptedVersion = requestedVersion
        guard save(updated,file) else { unlock(fd); event("uncertain","confirmation_state_write_failed",req); exit(1) }
        guard let (_,w2,rechecked) = bound(updated), CFEqual(w,w2), Date() < operationDeadline,
              let exact = researchStartAction(updated,rechecked,turn(rechecked,marker:updated.matchTarget,history:updated.matchHistory)),
              exact.1 == requestedVersion else {
            unlock(fd); event("uncertain","confirmation_context_changed_no_action",req); exit(1)
        }
        let accepted = press(rechecked.nodes[exact.0].element)
        if accepted {
            updated.confirmationPerformed = true
            guard save(updated,file) else { unlock(fd); event("uncertain","confirmation_state_write_failed_after_action",req); exit(1) }
        }
        unlock(fd)
        event(accepted ? "progress" : "uncertain",accepted ? "research_confirmation_submitted" : "research_confirmation_outcome_unknown",req)
        exit(accepted ? 0 : 1)
    } else {
        guard let (_,w,ui) = bound(b),
              let consent = toolConsentAction(b,ui,turn(ui,marker:b.matchTarget,history:b.matchHistory)),
              consent.version == requestedVersion else {
            unlock(fd); event("uncertain","confirmation_version_or_context_changed",req); exit(1)
        }
        var updated = b; updated.confirmationAttemptedVersion = requestedVersion
        guard save(updated,file) else { unlock(fd); event("uncertain","confirmation_state_write_failed",req); exit(1) }
        guard let (_,w2,rechecked) = bound(updated), CFEqual(w,w2), Date() < operationDeadline,
              let exact = toolConsentAction(updated,rechecked,turn(rechecked,marker:updated.matchTarget,history:updated.matchHistory)),
              exact.version == requestedVersion else {
            unlock(fd); event("uncertain","confirmation_context_changed_no_action",req); exit(1)
        }
        let targetIdx = requestedActionID == "tool_consent_allow" ? exact.allowIdx : exact.denyIdx
        let accepted = press(rechecked.nodes[targetIdx].element)
        if accepted {
            updated.confirmationPerformed = true
            guard save(updated,file) else { unlock(fd); event("uncertain","confirmation_state_write_failed_after_action",req); exit(1) }
        }
        unlock(fd)
        event(accepted ? "progress" : "uncertain",accepted ? "tool_consent_submitted" : "tool_consent_outcome_unknown",req)
        exit(accepted ? 0 : 1)
    }
}
if command == "collect" {
    guard let b = load(file), b.requestID == rid, b.sessionID == req["session_id"] as? String else {
        unlock(fd); event("uncertain","binding_missing_or_mismatch",req); exit(1)
    }
    if b.capability == "canvas" {
        emitTerminalIfPresent(b,file,fd,req)
        unlock(fd); event("uncertain","canvas_not_completed",req); exit(1)
    }
    if b.phase == "completed" && b.finalReason == "native_image_export_verified" {
        guard verifiedArtifacts(b,dir) != nil else {
            unlock(fd); event("uncertain","artifact_integrity_failed",req); exit(1)
        }
        unlock(fd); event("uncertain","native_download_provenance_unverified",req); exit(1)
    }
    emitTerminalIfPresent(b,file,fd,req)
    guard ["image","video"].contains(b.capability ?? ""), matchesLease(b,file),
          ["submit_pending","submitted","accepted"].contains(b.phase) else {
        unlock(fd); event("uncertain","artifact_not_recorded",req); exit(1)
    }
    // Continue this exact owned media operation through the observer below.
}
if command == "inspect" {
    guard let b = load(file), b.requestID == rid, b.sessionID == req["session_id"] as? String else {
        unlock(fd); event("uncertain","binding_missing_or_mismatch",req); exit(1)
    }
    let relation = leaseRelation(b,file)
    guard relation == .own || (relation == .absent && b.phase == "completed") else {
        unlock(fd); event("uncertain","active_binding_missing_or_mismatch",req); exit(1)
    }
    guard !nativeProcessGone(b), let (_,window,ui) = bound(b) else {
        if let (_,window) = boundWindow(b), hasDialog(window) {
            unlock(fd); event("waiting_human","authorization_account_or_quota_control",req); exit(0)
        }
        unlock(fd); event("uncertain","bound_window_or_ax_unavailable",req); exit(1)
    }
    _ = window
    if ui.dialog { unlock(fd); event("waiting_human","authorization_account_or_quota_control",req); exit(0) }
    let owned = turn(ui,marker:b.matchTarget,history:b.matchHistory)
    guard owned.owned else { unlock(fd); event("uncertain","owned_latest_turn_not_verified",req); exit(1) }
    let candidates = ownedAnswerRow(ui,b.matchTarget,owned.owned).map { candidateEvidence(ui.nodes,$0) } ?? []
    let descriptors = candidates.map { c -> [String:Any] in
        ["kind":c.kind,"count":c.count,"evidence":"ax_description_candidate",
         "live_verified":false,"artifact_id":NSNull(),"actionable":false]
    }
    let contentStatus = owned.readFailed ? "unreadable" : (owned.tooLarge ? "too_large" :
                        (owned.text == nil ? "not_observed" : "available"))
    var observation: [String:Any] = ["generation_active":ui.stop != nil,
        "answer_action_present":owned.copy,"content_status":contentStatus,
        "candidates":descriptors]
    if b.confirmationAttemptedVersion == nil, let pending = researchStartAction(b,ui,owned) {
        observation["pending_action"] = researchPending(pending.1)
    }
    unlock(fd)
    emit(["version":version,"kind":"progress","reason":"owned_state_observed",
          "request_id":rid,"session_id":b.sessionID,"completion_observed":false,
          "observation":observation])
    exit(0)
}
var binding: Binding
if command == "run" || command == "continue" {
    guard installed && trusted else { unlock(fd); event("error","app_accessibility_unavailable",req,notSubmitted:true); exit(1) }
    guard let (a, window) = ensureAppAndWindow(),
          let wid = windowID(window,a.processIdentifier),
          let launch = a.launchDate?.timeIntervalSince1970 else {
        unlock(fd); event("error","app_or_window_unavailable",req,notSubmitted:true); exit(1)
    }
    reclaimStaleLeaseIfSafe(window)
    let eligiblePrior = (req["prior_request_id"] as? String).flatMap { load(path(dir,$0)) }
    let mayHaveOwnedTool = eligiblePrior.map {
        provenPriorBinding($0) && $0.pid == a.processIdentifier && abs($0.launch-launch) < 0.01 && $0.window == wid
    } ?? false
    let isFreshRun = (command == "run" && req["prior_request_id"] == nil)
    var previous: Binding?
    var predecessor: Binding?
    var alreadyBlank = false
    var precheckedUI: UI? = nil
    if isFreshRun {
        let lw = lightweightNewChatHandle(window)
        if lw.ok {
            // Fast 5ms path: composer is empty and New Chat button is enabled; no need to walk old chat messages.
        } else if lw.reason == "new_chat_control_unavailable", let ui = snapshot(window),
                  ui.draft.isEmpty, !ui.dialog, !ui.hasAttachment, isAlreadyBlankChat(ui), ui.stop == nil {
            if ui.selectedTool, let cleared = clearStaleSelectedTool(window, ui) {
                precheckedUI = cleared
            } else {
                precheckedUI = ui
            }
            alreadyBlank = true
        } else {
            unlock(fd); event("error",lw.reason,req,notSubmitted:true); exit(1)
        }
    } else {
        if let rapid = quickStatus(window), rapid.reason == "generation_in_progress" ||
           (rapid.reason == "draft_present" && !mayHaveOwnedTool) {
            unlock(fd); event("error","busy:" + rapid.reason,req,notSubmitted:true); exit(1)
        }
        guard let ui = snapshot(window) else {
            unlock(fd); event("error","busy:ui_state_unavailable",req,notSubmitted:true); exit(1)
        }
        guard ui.stop == nil && ui.draft.isEmpty && !ui.dialog && !ui.hasAttachment else {
            unlock(fd); event("error","busy",req,notSubmitted:true); exit(1)
        }
        precheckedUI = ui
        if command == "run" {
            let priorID = req["prior_request_id"] as! String
            guard let old = load(path(dir,priorID)), old.requestID == priorID,
                  provenPriorBinding(old),
                  old.pid == a.processIdentifier, abs(old.launch-launch) < 0.01,
                  old.window == wid else {
                unlock(fd); event("uncertain","prior_binding_not_verified",req); exit(1)
            }
            if old.phase == "completed" && ["image","video"].contains(old.capability ?? "") && verifiedArtifacts(old,dir) == nil {
                unlock(fd); event("uncertain","prior_artifact_integrity_failed",req); exit(1)
            }
            guard sendContextOK(ui,"",old,mode:(old.capability ?? "chat")) else {
                unlock(fd); event("uncertain","prior_latest_turn_not_verified",req); exit(1)
            }
            if old.phase == "failed" {
                let observed = turn(ui,marker:old.matchTarget,history:old.matchHistory)
                guard decision(observed,stop:false) == .completed, observed.text == old.finalText else {
                    unlock(fd); event("uncertain","prior_latest_turn_not_verified",req); exit(1)
                }
            }
            predecessor = old
            guard let newChat = ui.newChat, enabled(ui.nodes[newChat].element) else {
                unlock(fd); event("error","new_chat_control_unavailable",req,notSubmitted:true); exit(1)
            }
        } else {
            let priorID = req["prior_request_id"] as! String
            guard priorID != rid, let old = load(path(dir,priorID)),
                  old.requestID == priorID, old.sessionID == (req["session_id"] as! String),
                  provenPriorBinding(old), (old.capability ?? "chat") == ((req["capability"] as? String) ?? "chat"),
                  old.pid == a.processIdentifier, abs(old.launch-launch) < 0.01,
                  old.window == wid else {
                unlock(fd); event("uncertain","prior_binding_not_verified",req); exit(1)
            }
            if ["image","video"].contains(old.capability ?? "") && verifiedArtifacts(old,dir) == nil {
                unlock(fd); event("uncertain","prior_artifact_integrity_failed",req); exit(1)
            }
            if !sendContextOK(ui,"",old,mode:(old.capability ?? "chat")) {
                if let reselectedUI = tryReselectPriorConversation(window, old: old) {
                    precheckedUI = reselectedUI
                } else {
                    unlock(fd); event("uncertain","prior_latest_turn_not_verified",req); exit(1)
                }
            }
            previous = old
        }
    }
    var leaseStat = stat()
    guard lstat(leaseFile, &leaseStat) != 0 && errno == ENOENT else {
        unlock(fd); event("error","active_binding_exists_or_lease_unavailable",req,notSubmitted:true); exit(1)
    }
    let session = req["session_id"] as! String
    let promptText = req["prompt"] as! String
    let marker = chatMarker(session,rid)
    let priorHistory = previous.map { $0.markerHistory ?? [$0.marker] } ?? []
    let priorPromptHistory = previous.map { $0.matchHistory } ?? []
    guard !priorHistory.contains(marker), priorHistory.count < 128 else {
        unlock(fd); event("error","turn_history_limit",req,notSubmitted:true); exit(1)
    }
    let requestedModel = canonicalDesktopModel(req["model"] as? String) ?? ""
    let requestedEffort = canonicalDesktopEffort(req["effort"] as? String) ?? ""
    binding = Binding(requestID:rid,sessionID:session,marker:marker,
                      pid:a.processIdentifier,launch:launch,window:wid,phase:"prepared",sawStop:false,
                      markerHistory:priorHistory + [marker],
                      promptText:promptText,
                      promptHistory:priorPromptHistory + [promptText],
                      sidebarTitle:previous?.sidebarTitle,
                      preSubmitRowCount:previous.flatMap { _ in precheckedUI.flatMap { chatSiblingRowCount($0) } },
                      priorRequestID:previous?.requestID,
                      capability:(req["capability"] as? String) ?? "chat",
                      requestedModel:requestedModel.isEmpty ? nil : requestedModel,
                      requestedEffort:requestedEffort.isEmpty ? nil : requestedEffort)
    let out = requestObj(req,binding)
    guard save(binding,file,create:true) else {
        unlock(fd); event("error","request_already_exists_or_state_unavailable",req,notSubmitted:true); exit(1)
    }
    guard createLease(Lease(stateFile:file,requestID:rid,sessionID:session)) else {
        _ = unlink(file)
        unlock(fd); event("error","active_binding_exists_or_lease_unavailable",out,notSubmitted:true); exit(1)
    }
    event("started","binding_persisted",out)
    var target: (AXUIElement, UI)?
    if command == "run" {
        if alreadyBlank, let ui = precheckedUI {
            target = (window, ui)
        } else {
            if predecessor == nil {
                guard Date() < operationDeadline else { abortBeforeSubmit(&binding,file,fd,out,"timeout_before_new_chat") }
                guard let (_, recheckedWindow) = boundWindow(binding) else {
                    abortBeforeSubmit(&binding,file,fd,out,"pre_newchat_window_lost")
                }
                let lw = lightweightNewChatHandle(recheckedWindow)
                guard lw.ok, let newChatButton = lw.button else {
                    abortBeforeSubmit(&binding,file,fd,out,"pre_newchat_" + lw.reason)
                }
                _ = AXUIElementPerformAction(newChatButton, kAXPressAction as CFString)
            } else {
                guard let ui = precheckedUI, Date() < operationDeadline else { abortBeforeSubmit(&binding,file,fd,out,"timeout_before_new_chat") }
                // Durable writes take time: recheck immediately before changing chats.
                guard let (_,recheckedWindow,rechecked) = bound(binding), CFEqual(recheckedWindow,window),
                      newChatContextOK(rechecked,ui,prior:predecessor),
                      let newChat = rechecked.newChat, enabled(rechecked.nodes[newChat].element) else {
                    abortBeforeSubmit(&binding,file,fd,out,"pre_newchat_context_changed")
                }
                // AX may report a failure after performing the action. Never replay:
                // instead reconcile by positively observing a fresh blank context.
                _ = AXUIElementPerformAction(rechecked.nodes[newChat].element,kAXPressAction as CFString)
            }
            // Allow SwiftUI to mount the zero-state New Chat hierarchy before sampling.
            _ = CFRunLoopRunInMode(.defaultMode,0.22,false)
            // Never send until the new chat is positively observed empty and settled.
            let freshDeadline = min(operationDeadline,Date().addingTimeInterval(6))
            for _ in 0..<22 {
                if Date() >= freshDeadline { break }
                if let (_,w,rawBlank) = bound(binding),
                   rawBlank.stop == nil, rawBlank.draft.isEmpty, !rawBlank.dialog, !rawBlank.hasAttachment,
                   !rawBlank.nodes.indices.contains(where: { isUserHeading(rawBlank.nodes,$0) || rawBlank.nodes[$0].ident == "copy-button" }) {
                    let blank = rawBlank.selectedTool ? (clearStaleSelectedTool(w, rawBlank) ?? rawBlank) : rawBlank
                    if !blank.selectedTool && chatModeSelected(blank) {
                        _ = CFRunLoopRunInMode(.defaultMode,0.12,false)
                        if let (_,w2,settled) = bound(binding), CFEqual(w,w2),
                           settled.stop == nil, settled.draft.isEmpty, !settled.dialog, !settled.hasAttachment, !settled.selectedTool,
                           !settled.nodes.indices.contains(where: { isUserHeading(settled.nodes,$0) || settled.nodes[$0].ident == "copy-button" }),
                           CFEqual(settled.nodes[settled.input].element, blank.nodes[blank.input].element) {
                            target = (w2,settled); break
                        }
                    }
                }
                _ = CFRunLoopRunInMode(.defaultMode,0.22,false)
            }
        }
    } else {
        target = precheckedUI.map { (window, $0) }
    }
    guard var (w,blank) = target else { abortBeforeSubmit(&binding,file,fd,out,"fresh_chat_not_verified") }
    let mode = binding.capability ?? "chat"
    if command == "continue" && mode == "deep_research", let old = previous {
        binding.priorResearchApprovalVersion = old.confirmationAttemptedVersion ?? old.priorResearchApprovalVersion
        binding.confirmationPerformed = old.confirmationPerformed
    }
    if ["canvas","deep_research"].contains(mode) {
        if command == "run" {
            // A blank chat must not already contain an unrelated document.
            binding.canvasBlankVerified = blank.nodes.indices.filter { blank.nodes[$0].role == "AXTextArea" }.count == 1
        } else if let old = previous, let hash = old.finalCanvasTextSHA {
            binding.canvasBlankVerified = true; binding.priorCanvasTextSHA = hash
        }
        guard save(binding,file) else { unlock(fd); event("uncertain","state_write_failed",out); exit(1) }
    }
    if command == "run" && ["image","video","deep_research","canvas"].contains(mode) {
        binding.phase = "mode_select_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","state_write_failed",out); exit(1) }
        guard let (_,beforeMenuWindow,beforeMenu) = bound(binding), CFEqual(beforeMenuWindow,w),
              (previous == nil || CFEqual(beforeMenu.nodes[beforeMenu.input].element,blank.nodes[blank.input].element)),
              sendContextOK(beforeMenu,"",nil) else {
            abortBeforeSubmit(&binding,file,fd,out,"pre_mode_context_changed")
        }
        let picker = beforeMenu.nodes.indices.filter { beforeMenu.nodes[$0].role == "AXButton" &&
            ["附加","Attach"].contains(beforeMenu.nodes[$0].desc) && enabled(beforeMenu.nodes[$0].element) }
        guard picker.count == 1, press(beforeMenu.nodes[picker[0]].element) else {
            abortBeforeSubmit(&binding,file,fd,out,mode + "_tool_picker_unavailable")
        }
        _ = CFRunLoopRunInMode(.defaultMode,0.4,false)
        guard let (_,menuWindow) = boundWindow(binding), CFEqual(menuWindow,w),
              let item = modeMenuItem(menuWindow,mode) else {
            abortBeforeSubmit(&binding,file,fd,out,mode + "_mode_menu_unavailable")
        }
        // The picker is open; verify the original composer remains empty and
        // focused on the same window before selecting the exact native item.
        let (draftStatus,draftValue) = attr(beforeMenu.nodes[beforeMenu.input].element,kAXValueAttribute)
        guard draftStatus == .success, (draftValue as? String) == "", Date() < operationDeadline else {
            abortBeforeSubmit(&binding,file,fd,out,mode + "_mode_context_changed")
        }
        guard press(item) else { abortBeforeSubmit(&binding,file,fd,out,mode + "_mode_selection_failed") }
        var selected: (AXUIElement,UI)?
        let until = min(operationDeadline,Date().addingTimeInterval(6))
        for _ in 0..<5 {
            if Date() >= until { break }
            if let (_,currentWindow,current) = bound(binding), CFEqual(currentWindow,w),
               sendContextOK(current,"",nil,mode:mode) {
                selected = (currentWindow,current); break
            }
            _ = CFRunLoopRunInMode(.defaultMode,0.3,false)
        }
        guard let (currentWindow,current) = selected else {
            abortBeforeSubmit(&binding,file,fd,out,mode + "_mode_not_verified")
        }
        w = currentWindow; blank = current
        binding.phase = "mode_selected"
        guard save(binding,file) else { unlock(fd); event("uncertain","state_write_failed",out); exit(1) }
        event("progress",mode + "_mode_selected",out)
    }
    if !requestedModel.isEmpty || !requestedEffort.isEmpty {
        if let selectorIdx = modelSelectorControl(blank.nodes),
           enabled(blank.nodes[selectorIdx].element) {
            let currentSelectorDesc = blank.nodes[selectorIdx].desc
            let modelAlreadyOK = requestedModel.isEmpty || selectorMatchesModel(currentSelectorDesc, canonicalModel: requestedModel)
            let effortAlreadyOK = requestedEffort.isEmpty || selectorMatchesEffort(currentSelectorDesc, canonicalEffort: requestedEffort)
            if !(modelAlreadyOK && requestedEffort.isEmpty) && !(modelAlreadyOK && effortAlreadyOK) {
                binding.phase = "model_select_pending"
                guard save(binding, file) else { unlock(fd); event("uncertain", "state_write_failed", out); exit(1) }
                guard let (_, beforeModelWindow, beforeModel) = bound(binding),
                      CFEqual(beforeModelWindow, w),
                      (previous == nil || CFEqual(beforeModel.nodes[beforeModel.input].element, blank.nodes[blank.input].element)),
                      sendContextOK(beforeModel, "", previous, mode: mode),
                      let liveSelectorIdx = modelSelectorControl(beforeModel.nodes),
                      enabled(beforeModel.nodes[liveSelectorIdx].element) else {
                    abortBeforeSubmit(&binding, file, fd, out, "pre_model_select_context_changed")
                }
                if !requestedModel.isEmpty && !selectorMatchesModel(beforeModel.nodes[liveSelectorIdx].desc, canonicalModel: requestedModel) {
                    guard press(beforeModel.nodes[liveSelectorIdx].element) else {
                        abortBeforeSubmit(&binding, file, fd, out, "model_selector_open_failed")
                    }
                    _ = CFRunLoopRunInMode(.defaultMode, 0.35, false)
                    guard let (liveApp, liveWindow) = boundWindow(binding), CFEqual(liveWindow, w) else {
                        abortBeforeSubmit(&binding, file, fd, out, "model_selector_window_lost")
                    }
                    var roots = pickerContainers(liveApp, liveWindow)
                    var modelItem = !roots.isEmpty ? findPickerItem(roots, candidates: modelDisplayCandidates(requestedModel)) : nil
                    if modelItem == nil && NSWorkspace.shared.frontmostApplication?.processIdentifier != binding.pid {
                        dismissPickerIfOpen(binding.pid)
                        _ = liveApp.activate()
                        _ = AXUIElementPerformAction(liveWindow, kAXRaiseAction as CFString)
                        _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
                        if let (_, fgWin, fgUI) = bound(binding), CFEqual(fgWin, w),
                           let fgSelIdx = modelSelectorControl(fgUI.nodes), enabled(fgUI.nodes[fgSelIdx].element) {
                            _ = press(fgUI.nodes[fgSelIdx].element)
                            _ = CFRunLoopRunInMode(.defaultMode, 0.35, false)
                            if let (fgApp2, fgWin2) = boundWindow(binding), CFEqual(fgWin2, w) {
                                roots = pickerContainers(fgApp2, fgWin2)
                                if !roots.isEmpty {
                                    modelItem = findPickerItem(roots, candidates: modelDisplayCandidates(requestedModel))
                                }
                            }
                        }
                    }
                    guard let chosenModelItem = modelItem else {
                        dismissPickerIfOpen(binding.pid)
                        _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
                        abortBeforeSubmit(&binding, file, fd, out, "model_option_not_found")
                    }
                    let (draftSt, draftVal) = attr(beforeModel.nodes[beforeModel.input].element, kAXValueAttribute)
                    guard draftSt == .success, (draftVal as? String) == "", Date() < operationDeadline else {
                        dismissPickerIfOpen(binding.pid)
                        abortBeforeSubmit(&binding, file, fd, out, "model_select_context_changed")
                    }
                    guard press(chosenModelItem) else {
                        dismissPickerIfOpen(binding.pid)
                        abortBeforeSubmit(&binding, file, fd, out, "model_option_press_failed")
                    }
                    _ = CFRunLoopRunInMode(.defaultMode, 0.35, false)
                    if let (checkApp, checkWin) = boundWindow(binding), !pickerContainers(checkApp, checkWin).isEmpty {
                        dismissPickerIfOpen(binding.pid)
                        _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
                    }
                    var verifiedModelUI: (AXUIElement, UI)?
                    let modelUntil = min(operationDeadline, Date().addingTimeInterval(4))
                    for _ in 0..<8 {
                        if Date() >= modelUntil { break }
                        if let (_, curWin, curUI) = bound(binding), CFEqual(curWin, w),
                           sendContextOK(curUI, "", previous, mode: mode),
                           let curSel = modelSelectorControl(curUI.nodes),
                           selectorMatchesModel(curUI.nodes[curSel].desc, canonicalModel: requestedModel) {
                            verifiedModelUI = (curWin, curUI)
                            break
                        }
                        _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
                    }
                    guard let (curWin, curUI) = verifiedModelUI else {
                        abortBeforeSubmit(&binding, file, fd, out, "model_selection_not_verified")
                    }
                    w = curWin; blank = curUI
                }
                if !requestedEffort.isEmpty {
                    if let effortSelIdx = modelSelectorControl(blank.nodes),
                       enabled(blank.nodes[effortSelIdx].element),
                       !selectorMatchesEffort(blank.nodes[effortSelIdx].desc, canonicalEffort: requestedEffort) {
                        if press(blank.nodes[effortSelIdx].element) {
                            _ = CFRunLoopRunInMode(.defaultMode, 0.35, false)
                            if let (liveApp, liveWindow) = boundWindow(binding), CFEqual(liveWindow, w) {
                                var roots = pickerContainers(liveApp, liveWindow)
                                if !roots.isEmpty {
                                    if let submenu = findPickerItem(roots, candidates: ["思考等级", "thinking level"], allowSubmenuHeader: true) {
                                        _ = press(submenu)
                                        _ = CFRunLoopRunInMode(.defaultMode, 0.3, false)
                                        roots = pickerContainers(liveApp, liveWindow)
                                    }
                                    if let effortItem = findPickerItem(roots, candidates: effortDisplayCandidates(requestedEffort)) {
                                        let (draftSt, draftVal) = attr(blank.nodes[blank.input].element, kAXValueAttribute)
                                        if draftSt == .success && (draftVal as? String) == "" && Date() < operationDeadline {
                                            _ = press(effortItem)
                                            _ = CFRunLoopRunInMode(.defaultMode, 0.35, false)
                                        }
                                    }
                                }
                            }
                            if let (checkApp, checkWin) = boundWindow(binding), !pickerContainers(checkApp, checkWin).isEmpty {
                                dismissPickerIfOpen(binding.pid)
                                _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
                            }
                            var settledAfterEffort: (AXUIElement, UI)?
                            let effortUntil = min(operationDeadline, Date().addingTimeInterval(3))
                            for _ in 0..<6 {
                                if Date() >= effortUntil { break }
                                if let (_, curWin, curUI) = bound(binding), CFEqual(curWin, w),
                                   sendContextOK(curUI, "", previous, mode: mode),
                                   (requestedModel.isEmpty || modelSelectorControl(curUI.nodes).map({ selectorMatchesModel(curUI.nodes[$0].desc, canonicalModel: requestedModel) }) == true) {
                                    settledAfterEffort = (curWin, curUI)
                                    break
                                }
                                _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
                            }
                            guard let (curWin, curUI) = settledAfterEffort else {
                                abortBeforeSubmit(&binding, file, fd, out, "post_effort_context_not_verified")
                            }
                            w = curWin; blank = curUI
                        }
                    }
                }
                binding.phase = "model_selected"
                guard save(binding, file) else { unlock(fd); event("uncertain", "state_write_failed", out); exit(1) }
                event("progress", "model_selected", out)
            }
        } else if !requestedModel.isEmpty {
            abortBeforeSubmit(&binding, file, fd, out, "model_selector_unavailable")
        }
    }
    let text = req["prompt"] as! String
    binding.phase = "input_pending"
    guard save(binding,file) else { unlock(fd); event("uncertain","state_write_failed",out); exit(1) }
    // Re-observe after the durable write, immediately before changing input.
    guard let (_,preWindow,prewrite) = bound(binding), CFEqual(preWindow,w),
          (previous == nil || CFEqual(prewrite.nodes[prewrite.input].element,blank.nodes[blank.input].element)),
          sendContextOK(prewrite,"",previous,mode:mode) else {
        abortBeforeSubmit(&binding,file,fd,out,"prewrite_context_changed")
    }
    guard AXUIElementSetAttributeValue(prewrite.nodes[prewrite.input].element,kAXValueAttribute as CFString,text as CFTypeRef) == .success else {
        abortBeforeSubmit(&binding,file,fd,out,"input_set_failed")
    }
    _ = CFRunLoopRunInMode(.defaultMode,0.08,false)
    guard let (_,filledWindow,filled) = bound(binding), CFEqual(filledWindow,w),
          sendContextOK(filled,text,previous,mode:mode) else {
        abortBeforeSubmit(&binding,file,fd,out,"input_not_verified")
    }
    binding.phase = "submit_pending"
    if let count = chatSiblingRowCount(filled) {
        binding.preSubmitRowCount = count
    }
    guard save(binding,file) else { unlock(fd); event("uncertain","state_write_failed",out); exit(1) }
    guard Date() < operationDeadline else { abortBeforeSubmit(&binding,file,fd,out,"timeout_before_submit") }
    // A second immediate check protects against navigation, a selected mode,
    // or an attachment appearing after typing but before the press.
    guard let (_,finalWindow,finalUI) = bound(binding), CFEqual(finalWindow,w),
          sendContextOK(finalUI,text,previous,mode:mode) else {
        abortBeforeSubmit(&binding,file,fd,out,"pre_send_context_changed")
    }
    let sends = finalUI.nodes.indices.filter { finalUI.nodes[$0].role == "AXButton" &&
        (["send_button","send-button"].contains(finalUI.nodes[$0].ident) || ["发送","发送消息","Send message","Send"].contains(finalUI.nodes[$0].desc)) &&
        enabled(finalUI.nodes[$0].element) }
    guard sends.count == 1 else { abortBeforeSubmit(&binding,file,fd,out,"send_control_unavailable") }
    guard press(finalUI.nodes[sends[0]].element) else { unlock(fd); event("uncertain","submit_action_failed",out); exit(1) }
    // Persist the fact of an attempted submission even if acceptance cannot yet be observed.
    binding.phase = "submitted"
    guard save(binding,file) else { unlock(fd); event("uncertain","submission_state_write_failed",out); exit(1) }
} else {
    guard let b = load(file), b.requestID == rid, b.sessionID == req["session_id"] as? String else {
        unlock(fd); event("uncertain","binding_missing_or_mismatch",req); exit(1)
    }
    binding = b
    emitTerminalIfPresent(b,file,fd,req)
    guard matchesLease(b,file) else { unlock(fd); event("uncertain","active_binding_missing_or_mismatch",req); exit(1) }
    if definitelyUnsent(b.phase) {
        // Both phases were durably recorded before any send action; the lock
        // proves no live helper can still be in that critical section.
        binding.phase = "not_submitted"
        binding.finalReason = "helper_exited_before_submit"
        binding.finalCompletion = false
        guard save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","pre_submit_state_not_reconciled",req); exit(1)
        }
        unlock(fd); event("error","helper_exited_before_submit",req,notSubmitted:true); exit(1)
    }
    if nativeProcessGone(binding) {
        binding.phase = "terminal_uncertain"; binding.finalReason = "native_process_gone_outcome_unknown"
        binding.finalCompletion = false
        guard save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","native_gone_state_not_reconciled",req); exit(1)
        }
        unlock(fd); event("uncertain","native_process_gone_outcome_unknown",req); exit(1)
    }
    guard installed && trusted else { unlock(fd); event("error","app_accessibility_unavailable",req); exit(1) }
    guard ["submit_pending","submitted","accepted","cancel_pending","cancel_sent"].contains(b.phase) else {
        unlock(fd); event("uncertain","submission_not_confirmed_no_replay",req); exit(1)
    }
}
unlock(fd)
let out = requestObj(req,binding)
let deadline = operationDeadline
var watcher: Watcher?
var lastText: String?
var lastProgressWrite = Date.distantPast
var expandAttempts = 0
var failures = 0
var retryDeadline: Date?
var unownedRetryDeadline: Date? = nil
func retryAllowed() -> Bool {
    if ["submitted","accepted"].contains(binding.phase) {
        usleep(250_000)
        return Date() < deadline
    }
    if retryDeadline == nil { retryDeadline = min(deadline,Date().addingTimeInterval(8)) }
    failures += 1
    usleep(150_000)
    return failures <= 40 && Date() < retryDeadline!
}
func waitForChange(_ watcher: Watcher?) {
    if let watcher = watcher { watcher.wait() }
    else { _ = CFRunLoopRunInMode(.defaultMode,0.3,false) }
}
while Date() < deadline {
    guard let fd = lock() else { usleep(100_000); continue }
    guard let current = load(file), current.sessionID == binding.sessionID,
          current.requestID == binding.requestID else {
        unlock(fd); event("uncertain","active_binding_missing_or_mismatch",out); exit(1)
    }
    // Another invocation may have stopped/completed the same turn. Read its
    // terminal receipt before requiring the already-released lease.
    emitTerminalIfPresent(current,file,fd,out)
    guard matchesLease(current,file) else {
        unlock(fd); event("uncertain","active_binding_missing_or_mismatch",out); exit(1)
    }
    guard ["submit_pending","submitted","accepted","cancel_pending","cancel_sent"].contains(current.phase) else {
        unlock(fd); event("uncertain","session_no_longer_active",out); exit(1)
    }
    binding = current
    if nativeProcessGone(binding) {
        binding.phase = "terminal_uncertain"; binding.finalReason = "native_process_gone_outcome_unknown"
        binding.finalCompletion = false
        guard save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","native_gone_state_not_reconciled",out); exit(1)
        }
        unlock(fd); event("uncertain","native_process_gone_outcome_unknown",out); exit(1)
    }
    if command == "cancel" && binding.mediaExportPhase == "goto_sent" {
        guard let (dialog,_,_) = mediaGoFolder(binding),
              let cancel = uniqueControl(dialog,roles:["AXButton"],labels:["Cancel","取消"]) else {
            unlock(fd); event("uncertain","owned_goto_cancel_unavailable",out); exit(1)
        }
        binding.mediaExportPhase = "goto_cancel_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_cancel_state_write_failed",out); exit(1) }
        guard let (dialog2,_,_) = mediaGoFolder(binding), CFEqual(dialog,dialog2), Date() < deadline,
              let exact = uniqueControl(dialog2,roles:["AXButton"],labels:["Cancel","取消"]), CFEqual(cancel,exact),
              NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid else {
            unlock(fd); event("uncertain","owned_goto_cancel_context_changed",out); exit(1)
        }
        guard press(exact) else { unlock(fd); event("uncertain","owned_goto_cancel_outcome_unknown",out); exit(1) }
        binding.mediaExportPhase = "goto_cancel_sent"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_cancel_state_write_failed",out); exit(1) }
        unlock(fd); waitForChange(watcher); continue
    }
    if command == "cancel" && binding.mediaExportPhase == "goto_cancel_sent" {
        guard let (sheet,_,_) = mediaSaveSheet(binding,requireEnabled:false), mediaNestedDialogCount(sheet) == 0 else {
            unlock(fd); event("uncertain","owned_goto_cancel_not_verified",out); exit(1)
        }
        binding.mediaExportPhase = "panel_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_cancel_state_write_failed",out); exit(1) }
    }
    if command == "cancel" && ["panel_pending","goto_committed"].contains(binding.mediaExportPhase ?? "") {
        guard let (sheet,_,_) = mediaSaveSheet(binding,requireEnabled:false), mediaNestedDialogCount(sheet) == 0,
              let cancel = uniqueControl(sheet,roles:["AXButton"],labels:["Cancel","取消"]) else {
            unlock(fd); event("uncertain","owned_save_cancel_unavailable",out); exit(1)
        }
        binding.mediaExportPhase = "panel_cancel_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_cancel_state_write_failed",out); exit(1) }
        guard let (sheet2,_,_) = mediaSaveSheet(binding,requireEnabled:false), CFEqual(sheet,sheet2),
              mediaNestedDialogCount(sheet2) == 0, Date() < deadline,
              let exact = uniqueControl(sheet2,roles:["AXButton"],labels:["Cancel","取消"]), CFEqual(cancel,exact),
              NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid else {
            unlock(fd); event("uncertain","owned_save_cancel_context_changed",out); exit(1)
        }
        guard press(exact) else { unlock(fd); event("uncertain","owned_save_cancel_outcome_unknown",out); exit(1) }
        binding.mediaExportPhase = "panel_cancel_sent"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_cancel_state_write_failed",out); exit(1) }
        unlock(fd); waitForChange(watcher); continue
    }
    if binding.mediaExportPhase == "panel_cancel_sent" {
        guard let (_,window) = boundWindow(binding) else {
            unlock(fd); event("uncertain","owned_save_cancel_window_lost",out); exit(1)
        }
        let (status,value) = attr(window,"AXSheets")
        guard status == .success, let sheets = value as? [AXUIElement], sheets.isEmpty,
              let (_,same,ui) = bound(binding), CFEqual(window,same), !ui.dialog,
              ui.stop == nil, ui.draft.isEmpty, !ui.hasAttachment else {
            unlock(fd); event("uncertain","owned_save_cancel_not_verified",out); exit(1)
        }
        let owned = turn(ui,marker:binding.marker,history:binding.markerHistory)
        guard owned.owned, !owned.readFailed, !owned.waiting else {
            unlock(fd); event("uncertain","owned_save_cancel_turn_unverified",out); exit(1)
        }
        binding.phase = "cancelled"; binding.finalReason = "cancelled"; binding.finalCompletion = false
        guard save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","cancel_state_not_reconciled",out); exit(1)
        }
        unlock(fd); event("completed","cancelled",out,completed:false); exit(0)
    }
    if ["open_pending","save_pending","goto_pending","goto_commit_pending",
        "goto_cancel_pending","panel_cancel_pending"].contains(binding.mediaExportPhase ?? "") {
        unlock(fd); event("uncertain","media_export_action_outcome_unknown",out); exit(1)
    }
    if binding.mediaExportPhase == "goto_sent" && command != "cancel" {
        guard let path = binding.mediaExportPath, let (dialog,field,_) = mediaGoFolder(binding) else {
            unlock(fd); event("uncertain","native_goto_folder_dialog_unavailable",out); exit(1)
        }
        let root = URL(fileURLWithPath:dir).appendingPathComponent("artifacts",isDirectory:true).standardizedFileURL.path
        guard URL(fileURLWithPath:path).deletingLastPathComponent().path == root, safeDir(root) else {
            unlock(fd); event("uncertain","media_destination_invalid",out); exit(1)
        }
        var settable = DarwinBoolean(false)
        guard AXUIElementIsAttributeSettable(field,kAXValueAttribute as CFString,&settable) == .success, settable.boolValue,
              AXUIElementSetAttributeValue(field,kAXValueAttribute as CFString,root as CFString) == .success,
              (attr(field,kAXValueAttribute).1 as? String) == root else {
            unlock(fd); event("uncertain","native_goto_folder_path_unverified",out); exit(1)
        }
        binding.mediaExportPhase = "goto_commit_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_save_state_write_failed",out); exit(1) }
        guard let (dialog2,field2,go) = mediaGoFolder(binding), CFEqual(dialog,dialog2),
              (attr(field2,kAXValueAttribute).1 as? String) == root, Date() < deadline,
              NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid else {
            unlock(fd); event("uncertain","native_goto_folder_changed_no_action",out); exit(1)
        }
        guard press(go) else { unlock(fd); event("uncertain","native_goto_folder_action_unknown",out); exit(1) }
        binding.mediaExportPhase = "goto_committed"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_save_state_write_failed",out); exit(1) }
        unlock(fd); waitForChange(watcher); continue
    }
    if binding.mediaExportPhase == "goto_committed" && command != "cancel" {
        guard let (sheet,_,_) = mediaSaveSheet(binding),
              mediaSheetDirectory(sheet) == URL(fileURLWithPath:dir).appendingPathComponent("artifacts",isDirectory:true).standardizedFileURL.path else {
            unlock(fd); event("uncertain","native_save_directory_unverified",out); exit(1)
        }
        binding.mediaExportPhase = "panel_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_save_state_write_failed",out); exit(1) }
    }
    if binding.mediaExportPhase == "panel_pending" && command != "cancel" {
        guard let path = binding.mediaExportPath, boundWindow(binding) != nil,
              let (sheet,field,_) = mediaSaveSheet(binding) else {
            unlock(fd); event("uncertain","owned_native_save_panel_unavailable",out); exit(1)
        }
        let root = URL(fileURLWithPath:dir).appendingPathComponent("artifacts",isDirectory:true).standardizedFileURL
        guard URL(fileURLWithPath:path).deletingLastPathComponent().path == root.path, safeDir(root.path) else {
            unlock(fd); event("uncertain","media_destination_invalid",out); exit(1)
        }
        if mediaSheetDirectory(sheet) != root.path {
            var canSet = DarwinBoolean(false)
            let canUseURL = AXUIElementIsAttributeSettable(sheet,"AXURL" as CFString,&canSet) == .success && canSet.boolValue
            if canUseURL {
                guard AXUIElementSetAttributeValue(sheet,"AXURL" as CFString,root as CFURL) == .success,
                      mediaSheetDirectory(sheet) == root.path else {
                    unlock(fd); event("uncertain","native_save_directory_unverified",out); exit(1)
                }
            } else {
                binding.mediaExportPhase = "goto_pending"
                guard save(binding,file) else { unlock(fd); event("uncertain","media_save_state_write_failed",out); exit(1) }
                guard let (sheet2,_,_) = mediaSaveSheet(binding), CFEqual(sheet,sheet2), Date() < deadline,
                      NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid,
                      let down = CGEvent(keyboardEventSource:nil,virtualKey:5,keyDown:true),
                      let up = CGEvent(keyboardEventSource:nil,virtualKey:5,keyDown:false) else {
                    unlock(fd); event("uncertain","native_goto_folder_context_changed_no_action",out); exit(1)
                }
                down.flags = [.maskCommand,.maskShift]; up.flags = [.maskCommand,.maskShift]
                down.postToPid(binding.pid); up.postToPid(binding.pid)
                binding.mediaExportPhase = "goto_sent"
                guard save(binding,file) else { unlock(fd); event("uncertain","media_save_state_write_failed",out); exit(1) }
                unlock(fd); waitForChange(watcher); continue
            }
        }
        let name = URL(fileURLWithPath:path).lastPathComponent
        var st = stat(), canSet = DarwinBoolean(false)
        guard lstat(path,&st) != 0 && errno == ENOENT,
              AXUIElementIsAttributeSettable(field,kAXValueAttribute as CFString,&canSet) == .success, canSet.boolValue,
              AXUIElementSetAttributeValue(field,kAXValueAttribute as CFString,name as CFString) == .success,
              (attr(field,kAXValueAttribute).1 as? String) == name else {
            unlock(fd); event("uncertain","native_save_name_unverified",out); exit(1)
        }
        binding.mediaExportPhase = "save_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_save_state_write_failed",out); exit(1) }
        guard let (sheet2,field2,button2) = mediaSaveSheet(binding), CFEqual(sheet,sheet2),
              mediaSheetDirectory(sheet2) == root.path, (attr(field2,kAXValueAttribute).1 as? String) == name,
              lstat(path,&st) != 0 && errno == ENOENT, Date() < deadline else {
            unlock(fd); event("uncertain","native_save_context_changed_no_action",out); exit(1)
        }
        guard press(button2) else { unlock(fd); event("uncertain","native_save_action_outcome_unknown",out); exit(1) }
        binding.mediaExportPhase = "save_sent"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_save_state_write_failed",out); exit(1) }
        unlock(fd); waitForChange(watcher); continue
    }
    guard let (_,w,ui) = bound(binding) else {
        if let (_,window) = boundWindow(binding), hasDialog(window) {
            unlock(fd); event("waiting_human","authorization_account_or_quota_control",out); exit(0)
        }
        if windowDefinitelyGone(binding) {
            unlock(fd); event("uncertain","bound_window_lost",out,
                              recovery:"when_safe_quit_original_app_then_resume"); exit(1)
        }
        guard retryAllowed() else {
            unlock(fd); event("uncertain",Date() >= deadline ? "timeout_generation_may_continue" : "ax_tree_unavailable",out); exit(1)
        }
        unlock(fd); waitForChange(watcher); continue
    }
    if watcher == nil { watcher = Watcher(binding.pid,w) }
    if ui.dialog { unlock(fd); event("waiting_human","authorization_account_or_quota_control",out); exit(0) }
    let t = turn(ui,marker:binding.matchTarget,history:binding.matchHistory)
    let outcome = decision(t,stop:ui.stop != nil)
    if outcome == .readFailed {
        guard retryAllowed() else {
            unlock(fd); event("uncertain",Date() >= deadline ? "timeout_generation_may_continue" : "response_read_unavailable",out); exit(1)
        }
        unlock(fd); waitForChange(watcher); continue
    }
    failures = 0; retryDeadline = nil
    if !t.owned {
        // Immediately after submission, the UI may not yet have rendered the turn.
        if mayMarkAccepted(binding.phase) && ui.draft.isEmpty && !ui.hasAttachment && command != "cancel" {
            unlock(fd); waitForChange(watcher); continue
        }
        // Tolerate brief SwiftUI layout transitions during active streaming when no
        // foreign heading, user draft, or attachment indicates a context change.
        if binding.phase == "accepted" && ui.draft.isEmpty && !ui.hasAttachment && command != "cancel" &&
           !hasForeignUserHeading(ui, expected: binding.matchHistory) {
            if unownedRetryDeadline == nil {
                unownedRetryDeadline = min(deadline, Date().addingTimeInterval(8.0))
            }
            if Date() < unownedRetryDeadline! {
                unlock(fd); waitForChange(watcher); continue
            }
        }
        if ui.stop == nil {
            _ = releaseLease(binding, file)
        }
        unlock(fd); event("uncertain","owned_latest_turn_not_verified",out); exit(1)
    }
    if binding.matchHistory.count > 12, mayMarkAccepted(binding.phase), ui.stop == nil,
       let preRows = binding.preSubmitRowCount, let curRows = chatSiblingRowCount(ui),
       curRows <= preRows {
        unlock(fd); waitForChange(watcher); continue
    }
    unownedRetryDeadline = nil
    if outcome == .waiting {
        if let consent = toolConsentAction(binding, ui, t) {
            if mayMarkAccepted(binding.phase) {
                binding.phase = "accepted"
                _ = save(binding, file)
            }
            if binding.confirmationAttemptedVersion != consent.version {
                unlock(fd)
                emit(["version":version,"kind":"waiting_human","reason":"tool_consent_confirmation",
                      "request_id":binding.requestID,"session_id":binding.sessionID,"completion_observed":false,
                      "pending_action":toolConsentPending(consent.version, consent.summary)])
                exit(0)
            }
        }
        unlock(fd); event("waiting_human","authorization_account_or_quota_control",out); exit(0)
    }
    if mayMarkAccepted(binding.phase) {
        binding.phase = "accepted"
        guard save(binding,file) else { unlock(fd); event("uncertain","acceptance_state_write_failed",out); exit(1) }
        event("progress","submitted_turn_observed",out)
    }
    if binding.phase == "accepted", binding.confirmationAttemptedVersion == nil,
       let pending = researchStartAction(binding,ui,t) {
        // A newly displayed plan needs its own confirmation even when this is
        // a continuation of an earlier confirmed research session.
        if binding.confirmationPerformed == true {
            binding.confirmationPerformed = false
            guard save(binding,file) else { unlock(fd); event("uncertain","research_plan_state_write_failed",out); exit(1) }
        }
        unlock(fd)
        emit(["version":version,"kind":"waiting_human","reason":"research_plan_confirmation",
              "request_id":binding.requestID,"session_id":binding.sessionID,"completion_observed":false,
              "pending_action":researchPending(pending.1)])
        exit(0)
    }
    if ui.stop != nil && !binding.sawStop {
        binding.sawStop = true
        guard save(binding,file) else { unlock(fd); event("uncertain","generation_state_write_failed",out); exit(1) }
    }
    let cancellation = cancelDecision(binding.phase,stop:ui.stop != nil,explicitCancel:command == "cancel")
    if cancellation != .inactive {
        if cancellation == .confirmed || cancellation == .unknown {
            let confirmed = cancellation == .confirmed
            binding.phase = confirmed ? "cancelled" : "terminal_uncertain"
            binding.finalReason = confirmed ? "cancelled" : "cancel_outcome_unknown"
            binding.finalCompletion = false
            guard save(binding,file), releaseLease(binding,file) else {
                unlock(fd); event("uncertain","cancel_state_not_reconciled",out); exit(1)
            }
            unlock(fd); event(confirmed ? "completed" : "uncertain",binding.finalReason!,out,completed:false)
            exit(confirmed ? 0 : 1)
        }
        if cancellation == .wait {
            unlock(fd); waitForChange(watcher); continue
        }
        if cancellation == .requiresCancel {
            unlock(fd); event("uncertain","cancel_pending_requires_explicit_cancel",out); exit(1)
        }
        // A prior stop attempt may have crashed before acknowledgement. Only
        // retry against the same verified owned generation and live stop control.
    }
    // A response may finish after Cancel was requested but before we inspect
    // it. Preserve the completed owned result before attempting any stop.
    if nativeImageFailure(binding,ui,t) {
        binding.phase = "failed"; binding.finalReason = "native_error_response"
        binding.finalText = t.text; binding.finalCompletion = false; binding.finalFailure = true
        guard save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","failure_state_not_reconciled",out); exit(1)
        }
        unlock(fd); event("failed","native_error_response",out,t.text,failureObserved:true); exit(1)
    }
    if ["image","video"].contains(binding.capability ?? "") && ui.stop == nil && t.owned && t.copy && command != "cancel" {
        if binding.mediaExportPhase == "save_sent" {
            guard let artifact = mediaArtifact(binding,dir) else {
                unlock(fd); event("uncertain","native_saved_artifact_not_verified",out); exit(1)
            }
            binding.artifacts = [artifact]; binding.finalExportProvenance = "owned_native_save_panel"
            binding.phase = "completed"; binding.finalReason = "native_media_export_verified"; binding.finalCompletion = true
            guard let verified = verifiedArtifacts(binding,dir), !verified.isEmpty,
                  save(binding,file), releaseLease(binding,file) else {
                unlock(fd); event("uncertain","native_media_completion_not_reconciled",out); exit(1)
            }
            unlock(fd); event("completed","native_media_export_verified",out,completed:true,artifacts:verified); exit(0)
        }
        guard binding.mediaExportPhase == nil, let download = mediaDownloadControl(binding,ui,t),
              NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid else {
            unlock(fd); event("uncertain","owned_media_download_unavailable",out); exit(1)
        }
        let root = URL(fileURLWithPath:dir).appendingPathComponent("artifacts",isDirectory:true).standardizedFileURL
        guard safeDir(root.path) else { unlock(fd); event("uncertain","artifact_directory_unavailable",out); exit(1) }
        let id = UUID().uuidString.lowercased(), ext = binding.capability == "image" ? "png" : "mp4"
        let destination = root.appendingPathComponent(id + "." + ext).path
        var st = stat()
        guard lstat(destination,&st) != 0 && errno == ENOENT else {
            unlock(fd); event("uncertain","media_destination_exists",out); exit(1)
        }
        binding.mediaExportID = id; binding.mediaExportPath = destination; binding.mediaExportPhase = "open_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_export_state_write_failed",out); exit(1) }
        guard let (_,w2,latest) = bound(binding), CFEqual(w,w2), Date() < deadline,
              NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid,
              let exact = mediaDownloadControl(binding,latest,turn(latest,marker:binding.matchTarget,history:binding.matchHistory)),
              CFEqual(latest.nodes[exact].element,ui.nodes[download].element) else {
            unlock(fd); event("uncertain","media_download_context_changed_no_action",out); exit(1)
        }
        guard press(latest.nodes[exact].element) else { unlock(fd); event("uncertain","media_download_action_outcome_unknown",out); exit(1) }
        binding.mediaExportPhase = "panel_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","media_export_state_write_failed",out); exit(1) }
        unlock(fd); waitForChange(watcher); continue
    }
    if ["canvas","deep_research"].contains(binding.capability ?? "") && ui.stop == nil && t.owned && t.copy && command != "cancel" {
        // A continuation starts with the previous document still visible.
        // It needs a verified close/disappearance before opening the new card,
        // even when its document text happens to be unchanged.
        let needsPriorClose = binding.priorCanvasTextSHA != nil && binding.canvasCloseVerified != true
        if binding.canvasBlankVerified != true || needsPriorClose {
            if binding.canvasCloseAttempted == nil {
                guard NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid,
                      let closeIndex = canvasCloseControl(binding,ui) else {
                    unlock(fd); event("uncertain","canvas_close_control_unverified",out); exit(1)
                }
                binding.canvasCloseAttempted = true
                guard save(binding,file) else { unlock(fd); event("uncertain","canvas_close_state_write_failed",out); exit(1) }
                guard let (_,w2,latest) = bound(binding), CFEqual(w,w2), Date() < deadline,
                      NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid,
                      let exact = canvasCloseControl(binding,latest),
                      CFEqual(latest.nodes[exact].element,ui.nodes[closeIndex].element) else {
                    unlock(fd); event("uncertain","canvas_close_context_changed_no_action",out); exit(1)
                }
                guard press(latest.nodes[exact].element) else { unlock(fd); event("uncertain","canvas_close_action_unknown",out); exit(1) }
                binding.canvasClosePerformed = true
                guard save(binding,file) else { unlock(fd); event("uncertain","canvas_close_state_write_failed",out); exit(1) }
                unlock(fd); waitForChange(watcher); continue
            }
            guard binding.canvasClosePerformed == true, let areas = canvasSideAreas(binding,ui), areas.isEmpty else {
                unlock(fd); event("uncertain","canvas_baseline_unverified",out); exit(1)
            }
            binding.canvasBlankVerified = true; binding.canvasCloseVerified = true
            guard save(binding,file) else { unlock(fd); event("uncertain","canvas_baseline_state_write_failed",out); exit(1) }
        }
        if binding.canvasOpenAttempted == nil {
            guard NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid,
                  let card = canvasCard(binding,ui), let frame = nodeFrame(w), frame.contains(card.2) else {
                unlock(fd); event("uncertain","canvas_card_or_foreground_unverified",out); exit(1)
            }
            var hit: AXUIElement?, hitPID: pid_t = 0
            guard AXUIElementCopyElementAtPosition(AXUIElementCreateSystemWide(),Float(card.2.x),Float(card.2.y),&hit) == .success,
                  let target = hit, CFEqual(target,ui.nodes[card.0].element),
                  AXUIElementGetPid(target,&hitPID) == .success, hitPID == binding.pid else {
                unlock(fd); event("uncertain","canvas_card_hit_unverified",out); exit(1)
            }
            binding.canvasOpenAttempted = true
            guard save(binding,file) else { unlock(fd); event("uncertain","canvas_open_state_write_failed",out); exit(1) }
            // Recheck the same card, window, PID, latest turn and frontmost hit
            // after the durable write and before the one allowed click.
            guard let (_,w2,latest) = bound(binding), CFEqual(w,w2), Date() < deadline,
                  NSWorkspace.shared.frontmostApplication?.processIdentifier == binding.pid,
                  let exact = canvasCard(binding,latest), exact.1 == card.1,
                  CFEqual(latest.nodes[exact.0].element,ui.nodes[card.0].element),
                  let bounds = nodeFrame(w2), bounds.contains(exact.2) else {
                unlock(fd); event("uncertain","canvas_card_changed_no_action",out); exit(1)
            }
            var freshHit: AXUIElement?, freshPID: pid_t = 0
            guard AXUIElementCopyElementAtPosition(AXUIElementCreateSystemWide(),Float(exact.2.x),Float(exact.2.y),&freshHit) == .success,
                  let h = freshHit, CFEqual(h,latest.nodes[exact.0].element),
                  AXUIElementGetPid(h,&freshPID) == .success, freshPID == binding.pid,
                  let down = CGEvent(mouseEventSource:nil,mouseType:.leftMouseDown,mouseCursorPosition:exact.2,mouseButton:.left),
                  let up = CGEvent(mouseEventSource:nil,mouseType:.leftMouseUp,mouseCursorPosition:exact.2,mouseButton:.left) else {
                unlock(fd); event("uncertain","canvas_card_hit_changed_no_action",out); exit(1)
            }
            down.post(tap:.cghidEventTap); up.post(tap:.cghidEventTap)
            binding.canvasOpenPerformed = true; binding.finalCanvasTitle = exact.1
            guard save(binding,file) else { unlock(fd); event("uncertain","canvas_open_outcome_unknown",out); exit(1) }
            _ = CFRunLoopRunInMode(.defaultMode,0.4,false)
        }
        guard binding.canvasOpenPerformed == true, let title = binding.finalCanvasTitle,
              let (_,wa,first) = bound(binding), CFEqual(w,wa),
              let document = canvasDocument(binding,first,title),
              (binding.priorCanvasTextSHA == nil || binding.canvasCloseVerified == true),
              let (_,wb,second) = bound(binding), CFEqual(w,wb),
              canvasDocument(binding,second,title) == document else {
            unlock(fd); event("uncertain","canvas_document_not_verified",out); exit(1)
        }
        let outputKind = binding.capability == "deep_research" ? "research_report_text" : "canvas_document_text"
        binding.phase = "completed"; binding.finalReason = outputKind
        binding.finalText = document; binding.finalCanvasTextSHA = sha256Text(document)
        binding.finalCanvasLocalID = canvasLocalID(title,document); binding.finalCompletion = true
        guard let evidence = canvasEvidence(binding), save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","canvas_completion_not_reconciled",out); exit(1)
        }
        unlock(fd); event("completed",outputKind,out,document,completed:true,completionEvidence:evidence); exit(0)
    }
    if binding.capability == "video" && ui.stop == nil && t.owned && t.copy {
        unlock(fd); event("uncertain",(binding.capability ?? "media") + "_output_unverified",out,t.text); exit(1)
    }
    if mayFinalizeBeforeStop(outcome,binding.phase), outcome == .tooLarge {
        binding.phase = "terminal_uncertain"; binding.finalReason = "response_too_large"
        binding.finalCompletion = false
        guard save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","oversize_state_not_reconciled",out); exit(1)
        }
        unlock(fd); event("uncertain","response_too_large",out); exit(1)
    }
    if (binding.capability ?? "chat") == "chat", ui.stop == nil, t.owned, t.copy, !t.readFailed,
       outcome == .pending, expandAttempts < 3,
       let row = ownedAnswerRow(ui, binding.matchTarget, true) {
        let expandLabels: Set<String> = ["展开", "显示更多", "Show more", "Expand"]
        let expandButtons = ui.nodes.indices.filter { idx in
            inside(ui.nodes, idx, row) &&
            ui.nodes[idx].role == "AXButton" &&
            expandLabels.contains(ui.nodes[idx].desc) &&
            enabled(ui.nodes[idx].element)
        }
        if expandButtons.count == 1 {
            expandAttempts += 1
            _ = press(ui.nodes[expandButtons[0]].element)
            unlock(fd)
            _ = CFRunLoopRunInMode(.defaultMode, 0.25, false)
            continue
        }
    }
    if (binding.capability ?? "chat") == "chat", mayFinalizeBeforeStop(outcome,binding.phase), outcome == .completed, let text = t.text {
        binding.phase = "completed"; binding.finalReason = "owned_response_action_observed"
        binding.finalText = text; binding.finalCompletion = true
        if let selectedTitle = selectedSidebarConversationTitle(w) {
            binding.sidebarTitle = selectedTitle
        }
        clearProgressSnapshot(dir, binding.requestID)
        guard save(binding,file), releaseLease(binding,file) else {
            unlock(fd); event("uncertain","completion_state_not_reconciled",out); exit(1)
        }
        unlock(fd); event("completed","owned_response_action_observed",out,text,completed:true); exit(0)
    }
    if command == "cancel" {
        guard let stop = ui.stop, enabled(ui.nodes[stop].element) else {
            unlock(fd); event("uncertain","owned_active_generation_not_observed",out); exit(1)
        }
        guard Date() < deadline else { unlock(fd); event("uncertain","timeout_generation_may_continue",out); exit(1) }
        binding.phase = "cancel_pending"
        guard save(binding,file) else { unlock(fd); event("uncertain","state_write_failed",out); exit(1) }
        guard press(ui.nodes[stop].element) else { unlock(fd); event("uncertain","stop_action_failed",out); exit(1) }
        binding.phase = "cancel_sent"
        guard save(binding,file) else { unlock(fd); event("uncertain","stop_state_write_failed",out); exit(1) }
        unlock(fd); waitForChange(watcher); continue
    }
    let streamText: String? = (t.text?.isEmpty == false)
        ? t.text
        : ((binding.capability ?? "chat") == "chat" && ui.stop != nil && t.owned && !t.waiting && !t.readFailed
            ? streamingPreviewText(ui, marker: binding.matchTarget)
            : nil)
    if (binding.capability ?? "chat") == "chat", t.owned, !t.readFailed,
       let text = streamText, !text.isEmpty, text != lastText {
        let now = Date()
        if now.timeIntervalSince(lastProgressWrite) >= 0.3 {
            writeProgressSnapshot(dir, binding, text)
            lastProgressWrite = now
            lastText = text
            event("progress","owned_response",out,text)
        }
    }
    unlock(fd); waitForChange(watcher)
}
clearProgressSnapshot(dir, binding.requestID)
event("uncertain","timeout_generation_may_continue",out)
exit(1)
