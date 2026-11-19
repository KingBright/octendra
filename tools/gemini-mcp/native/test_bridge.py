"""Deterministic AX ownership and lifecycle fixtures; never submits a Gemini turn."""
import json
import os
import pathlib
import subprocess
import tempfile
import unittest

SOURCE = pathlib.Path(__file__).with_name("bridge.swift")
FIXTURE = r'''
let fake = AXUIElementCreateSystemWide()
func node(_ role: String, _ id: String = "", _ desc: String = "", _ parent: Int) -> Node {
    Node(element:fake, role:role, ident:id, desc:desc, parent:parent)
}
let marker = "[GeminiDesktop request fixture]"
// Neither a missing stop ID nor a failed stop description is a valid tree:
// such a snapshot can neither prove completion nor authorize cancellation.
if let _ = validatedMetadata(.success,"AXButton",.invalidUIElement,nil,.success,"停止回答") {
    fatalError("invalid stop ID accepted")
}
if let _ = validatedMetadata(.success,"AXButton",.success,"stop_button",.cannotComplete,nil) {
    fatalError("failed stop description accepted")
}
if let _ = validatedMetadata(.success,"AXHeading",.cannotComplete,nil,.success,marker) {
    fatalError("failed user marker metadata accepted")
}
if let _ = validatedMetadata(.success,"AXButton",.success,"copy-button",.invalidUIElement,nil) {
    fatalError("failed copy metadata accepted")
}
if let _ = validatedMetadata(.success,"AXButton",.success,"tool-consent-allow",.cannotComplete,nil) {
    fatalError("failed authorization metadata accepted")
}
let absentMetadata = validatedMetadata(.success,"AXButton",.attributeUnsupported,nil,.noValue,nil)
precondition(absentMetadata?.1 == "" && absentMetadata?.2 == "")
let composer141 = validatedMetadata(.success,"AXTextArea",.failure,nil,.success,"接下来做什么？")
precondition(composer141?.1 == "")
precondition(passiveTextLeaf("AXGroup",.failure,"passive text",0,.success,["AXShowMenu"],.success,"passive text"))
precondition(!passiveTextLeaf("AXGroup",.cannotComplete,"passive text",0,.success,["AXShowMenu"],.success,"passive text"))
precondition(!passiveTextLeaf("AXGroup",.failure,"passive text",0,.success,["AXPress"],.success,"passive text"))
precondition(!passiveTextLeaf("AXGroup",.failure,"passive text",1,.success,["AXShowMenu"],.success,"passive text"))
precondition(passiveReadOnlyTextArea("AXTextArea",.failure,.failure,true,.success,["AXShowMenu"],.success,"canvas",.success,false))
precondition(!passiveReadOnlyTextArea("AXTextArea",.failure,.failure,true,.success,["AXShowMenu"],.success,"draft",.success,true))
precondition(!passiveReadOnlyTextArea("AXTextArea",.invalidUIElement,.failure,true,.success,["AXShowMenu"],.success,"",.success,false))
precondition(!passiveReadOnlyTextArea("AXTextArea",.failure,.failure,true,.success,["AXPress"],.success,"",.success,false))
precondition(!passiveReadOnlyTextArea("AXTextArea",.failure,.failure,false,.success,["AXShowMenu"],.success,"",.success,false))
if let _ = validatedMetadata(.success,"AXTextArea",.invalidUIElement,nil,.success,"接下来做什么？") {
    fatalError("invalid composer accepted")
}
if let _ = validatedMetadata(.success,"AXButton",.failure,nil,.success,"停止回答") {
    fatalError("failed stop metadata accepted")
}
precondition(isDraftControl("AXButton","共享屏幕",""))
precondition(!isDraftControl("AXButton","共享屏幕","",help:"选择“屏幕上的内容”选项。"))
precondition(!isDraftControl("AXButton","Share screen","",help:"Select screen context options."))
precondition(isDraftControl("AXButton","共享屏幕","",help:"unexpected"))
precondition(isDraftControl("AXButton","移除",""))
precondition(!isDraftControl("AXStaticText","移除",""))
var nodes = [
    node("AXWindow", "", "", -1), node("AXGroup", "", "", 0),
    node("AXRow", "", "", 1), node("AXRow", "", "", 1),
    node("AXHeading", "userMessage.expandButton", marker + " prompt", 2),
    node("AXGroup", "", "", 3), node("AXGroup", "", "", 5),
    node("AXGroup", "", "", 5), node("AXStaticText", "", "", 6),
    node("AXButton", "copy-button", "复制回答", 7), node("AXTextArea", "", "", 0),
]
func makeUI(_ ns: [Node], _ draft: String = "", _ dialog: Bool = false) -> UI {
    UI(nodes:ns,input:10,stop:nil,newChat:nil,draft:draft,dialog:dialog)
}
func read(_ i: Int) -> String? { i == 8 ? "普通回答 Allow once 允许一次" : "" }
var canvasHeadingNodes = nodes
canvasHeadingNodes[4].ident = "userMessage.expandButton-userMessage.expandButton"
precondition(isUserHeading(canvasHeadingNodes,4))
precondition(turn(makeUI(canvasHeadingNodes),marker:marker,reader:read).owned)
var t = turn(makeUI(nodes),marker:marker,reader:read)
precondition(t.owned && t.copy && t.text == "普通回答 Allow once 允许一次" && !t.waiting)
precondition(decision(t,stop:false) == .completed)
precondition(decision(t,stop:true) == .progress)
var partial = nodes
partial.append(node("AXStaticText","","",6))
let failedRead = turn(makeUI(partial),marker:marker,reader:{
    if $0 == 8 { return "first fragment" }
    if $0 == 11 { return nil }
    return ""
})
precondition(failedRead.owned && failedRead.copy && failedRead.readFailed)
precondition(failedRead.text == nil && decision(failedRead,stop:false) == .readFailed)
precondition(definitelyUnsent("prepared") && definitelyUnsent("input_pending"))
precondition(!definitelyUnsent("submit_pending") && !definitelyUnsent("submitted"))
precondition(mayMarkAccepted("submitted") && mayMarkAccepted("submit_pending"))
precondition(!mayMarkAccepted("cancel_pending") && !mayMarkAccepted("cancel_sent"))
precondition(mayFinalizeBeforeStop(.completed,"accepted"))
precondition(mayFinalizeBeforeStop(.tooLarge,"accepted"))
precondition(!mayFinalizeBeforeStop(.completed,"cancel_pending"))
precondition(!mayFinalizeBeforeStop(.completed,"cancel_sent"))
precondition(!mayFinalizeBeforeStop(.progress,"accepted"))
precondition(cancelDecision("cancel_pending",stop:true,explicitCancel:true) == .retry)
precondition(cancelDecision("cancel_pending",stop:true,explicitCancel:false) == .requiresCancel)
precondition(cancelDecision("cancel_pending",stop:false,explicitCancel:false) == .unknown)
precondition(cancelDecision("cancel_sent",stop:true,explicitCancel:false) == .wait)
precondition(cancelDecision("cancel_sent",stop:false,explicitCancel:false) == .confirmed)
precondition(processGone(true,false,nil,1))
precondition(processGone(false,true,1,1))
precondition(processGone(false,false,2,1))
precondition(!processGone(false,false,nil,1))
precondition(!processGone(false,false,1,1))
var rich = nodes
rich.append(node("AXLink","","link text",6))
rich.append(node("AXHeading","","section",6))
let richTurn = turn(makeUI(rich),marker:marker,reader:read)
precondition(decision(richTurn,stop:false) == .completed)
precondition(richTurn.text == "普通回答 Allow once 允许一次\nlink text\nsection")
rich.append(node("AXButton","","显示更多",6))
precondition(decision(turn(makeUI(rich),marker:marker,reader:read),stop:false) == .pending)
precondition(decision(turn(makeUI(nodes,"draft"),marker:marker,reader:read),stop:true) == .unowned)
var echoed = nodes
echoed[4].desc = "another request"
echoed[8].desc = marker
precondition(!turn(makeUI(echoed),marker:marker,reader:read).owned)
var later = nodes
later.append(node("AXRow","","",1))
later.append(node("AXHeading","userMessage.expandButton","manual followup",11))
precondition(!turn(makeUI(later),marker:marker,reader:read).owned)
// Typing agent text must never hide an attachment or selected tool.
var guardedNodes = nodes
guardedNodes[10].desc = "问问 Gemini"
guardedNodes.append(node("AXButton","chat-input-mode-ask","对话",0))
let old = Binding(requestID:"parent",sessionID:"session",marker:marker,pid:42,launch:1,window:2,
                  phase:"completed",sawStop:false,finalReason:"owned_response_action_observed",
                  finalText:"普通回答 Allow once 允许一次",finalCompletion:true)
let blankGuard = UI(nodes:guardedNodes,input:10,stop:nil,newChat:nil,draft:"",dialog:false)
precondition(newChatContextOK(blankGuard,blankGuard,selectedReader:{ _ in true }))
var draftRace = blankGuard
draftRace = UI(nodes:guardedNodes,input:10,stop:nil,newChat:nil,draft:"new user draft",dialog:false)
precondition(!newChatContextOK(draftRace,blankGuard,selectedReader:{ _ in true }))
var chipRace = blankGuard
chipRace.hasAttachment = true
precondition(!newChatContextOK(chipRace,blankGuard,selectedReader:{ _ in true }))
var switchedNodes = guardedNodes
switchedNodes[4].desc = "different user conversation"
let switchedUI = UI(nodes:switchedNodes,input:10,stop:nil,newChat:nil,draft:"",dialog:false)
precondition(!newChatContextOK(switchedUI,blankGuard,selectedReader:{ _ in true }))
var failureNodes = guardedNodes
failureNodes[10].desc = "描述要创作的图片"
let failureUI = UI(nodes:failureNodes,input:10,stop:nil,newChat:nil,draft:"",dialog:false,selectedTool:true)
var failureBinding = Binding(requestID:"image",sessionID:"session",marker:marker,pid:42,launch:1,window:2,
                             phase:"accepted",sawStop:true,capability:"image")
let failureTurn = Turn(owned:true,text:knownImageAccessError,copy:true,waiting:false)
precondition(nativeImageFailure(failureBinding,failureUI,failureTurn))
var pendingNodes = failureNodes
pendingNodes.append(node("AXProgressIndicator","","",0))
let pendingUI = UI(nodes:pendingNodes,input:10,stop:nil,newChat:nil,draft:"",dialog:false,selectedTool:true)
precondition(!nativeImageFailure(failureBinding,pendingUI,failureTurn))
let altered = Turn(owned:true,text:"Quoted: " + knownImageAccessError,copy:true,waiting:false)
precondition(!nativeImageFailure(failureBinding,failureUI,altered))
let stoppedUI = UI(nodes:failureNodes,input:10,stop:0,newChat:nil,draft:"",dialog:false,selectedTool:true)
precondition(!nativeImageFailure(failureBinding,stoppedUI,failureTurn))
failureBinding.phase = "failed"; failureBinding.finalReason = "native_error_response"
failureBinding.finalCompletion = false; failureBinding.finalFailure = true; failureBinding.finalText = knownImageAccessError
precondition(terminalReceipt(failureBinding)?.kind == "failed")
precondition(terminalReceipt(failureBinding)?.failureObserved == true)
var researchNodes = nodes
researchNodes.append(node("AXButton","","Start research",6))
let researchTurnUI = makeUI(researchNodes)
let researchBinding = Binding(requestID:"research",sessionID:"session",marker:marker,pid:42,launch:1,window:2,phase:"accepted",sawStop:false,capability:"deep_research")
let researchTurn = Turn(owned:true,text:nil,copy:false,waiting:false)
let pendingA = researchStartAction(researchBinding,researchTurnUI,researchTurn,valueReader:{ $0 == 8 ? "plan one" : "" },enabledReader:{ _ in true })!
let pendingB = researchStartAction(researchBinding,researchTurnUI,researchTurn,valueReader:{ $0 == 8 ? "plan two" : "" },enabledReader:{ _ in true })!
precondition(pendingA.1 != pendingB.1 && pendingA.1.count == 64)
precondition(researchStartAction(researchBinding,researchTurnUI,researchTurn,valueReader:{ _ in nil },enabledReader:{ _ in true }) == nil)
precondition(researchStartAction(researchBinding,researchTurnUI,researchTurn,valueReader:{ _ in "" },enabledReader:{ _ in false }) == nil)
precondition(provenPriorBinding(failureBinding))
var badFailure = failureBinding
badFailure.finalText = "different"
precondition(!provenPriorBinding(badFailure))
let safeDraft = UI(nodes:guardedNodes,input:10,stop:nil,newChat:nil,draft:"agent text",dialog:false)
precondition(sendContextOK(safeDraft,"agent text",old,reader:read,selectedReader:{ _ in true }))
precondition(!sendContextOK(safeDraft,"agent text",old,reader:read,selectedReader:{ _ in false }))
var withAttachment = safeDraft
withAttachment.hasAttachment = true
precondition(!sendContextOK(withAttachment,"agent text",old,reader:read,selectedReader:{ _ in true }))
var wrongModeNodes = guardedNodes
wrongModeNodes[10].desc = "描述要创作的图片"
let wrongMode = UI(nodes:wrongModeNodes,input:10,stop:nil,newChat:nil,draft:"agent text",dialog:false)
precondition(!sendContextOK(wrongMode,"agent text",old,reader:read,selectedReader:{ _ in true }))
let imageNodes = [node("AXTextArea","","描述要创作的图片",-1)]
var imageUI = UI(nodes:imageNodes,input:0,stop:nil,newChat:nil,draft:"",dialog:false)
precondition(!sendContextOK(imageUI,"",nil,mode:"image"))
imageUI.selectedTool = true
precondition(sendContextOK(imageUI,"",nil,mode:"image"))
imageUI.hasAttachment = true
precondition(!sendContextOK(imageUI,"",nil,mode:"image"))
var canvasUI = UI(nodes:[node("AXTextArea","","一起开始创作吧",-1)],input:0,stop:nil,newChat:nil,draft:"",dialog:false,selectedTool:true)
precondition(sendContextOK(canvasUI,"",nil,mode:"canvas"))
precondition(!sendContextOK(canvasUI,"",nil,mode:"image"))
canvasUI.hasAttachment = true
precondition(!sendContextOK(canvasUI,"",nil,mode:"canvas"))
let videoUI = UI(nodes:[node("AXTextArea","","描述要创作的视频",-1)],input:0,stop:nil,newChat:nil,draft:"",dialog:false,selectedTool:true)
precondition(sendContextOK(videoUI,"",nil,mode:"video"))
precondition(!sendContextOK(videoUI,"",nil,mode:"deep_research"))
let researchUI = UI(nodes:[node("AXTextArea","","你想研究什么？",-1)],input:0,stop:nil,newChat:nil,draft:"",dialog:false,selectedTool:true)
precondition(sendContextOK(researchUI,"",nil,mode:"deep_research"))
precondition(!definitelyUnsent("submitted") && definitelyUnsent("mode_select_pending") && definitelyUnsent("model_select_pending") && definitelyUnsent("model_selected"))
precondition(canonicalDesktopModel(nil) == "" && canonicalDesktopModel("auto") == "" && canonicalDesktopModel("gemini-desktop-task") == "")
precondition(canonicalDesktopModel("Argon") == "argon" && canonicalDesktopModel("gemini-4-argon") == "argon")
precondition(canonicalDesktopModel("Carbon") == "carbon" && canonicalDesktopModel("gemini_app/gemini-4-carbon-high") == "carbon")
precondition(canonicalDesktopModel("gemini-4-hydrogen") == "flash" && canonicalDesktopModel("Flash") == "flash")
precondition(canonicalDesktopModel("gemini-4-neon") == "neon" && canonicalDesktopModel("gemini-4-neon-xhigh") == "neon")
precondition(canonicalDesktopModel("gemini-5-xenon") == "xenon" && canonicalDesktopModel("gemini-desktop-auto") == "desktop_auto")
precondition(pickerTextsMatch(["deeper analysis、4 neon"], candidates: modelDisplayCandidates("neon")).exact)
precondition(pickerTextsMatch(["日常帮助、4 hydrogen"], candidates: modelDisplayCandidates("flash")).exact)
precondition((parseDiscoveredModelOption(["Deeper analysis、4 Neon"])?["id"] as? String) == "gemini-4-neon")
precondition(canonicalDesktopModel("unknown-model") == nil)
precondition(canonicalDesktopEffort(nil) == "" && canonicalDesktopEffort("default") == "")
precondition(canonicalDesktopEffort("low") == "low" && canonicalDesktopEffort("Extended") == "high" && canonicalDesktopEffort("max") == "max")
precondition(canonicalDesktopEffort("ultra-999") == nil)
precondition(selectorMatchesModel("Carbon，选择模型", canonicalModel: "carbon"))
precondition(selectorMatchesModel("Flash Extended, Choose model", canonicalModel: "flash"))
precondition(!selectorMatchesModel("Carbon，选择模型", canonicalModel: "argon"))
precondition(selectorMatchesEffort("Flash Extended, Choose model", canonicalEffort: "high"))
let selectorNodes = [node("AXWindow","","",-1), node("AXButton","tab_stop_model_selector","Carbon，选择模型",0)]
precondition(modelSelectorControl(selectorNodes) == 1)
var navigatedNodes = guardedNodes
navigatedNodes[4].desc = "different conversation"
let navigated = UI(nodes:navigatedNodes,input:10,stop:nil,newChat:nil,draft:"agent text",dialog:false)
precondition(!sendContextOK(navigated,"agent text",old,reader:read,selectedReader:{ _ in true }))
// The fresh Ask selector is absent after a response. Only a verified prior
// native-chat binding may provide inherited mode evidence.
var noSelectorNodes = guardedNodes
noSelectorNodes.removeLast()
let noSelector = UI(nodes:noSelectorNodes,input:10,stop:nil,newChat:nil,draft:"agent text",dialog:false)
precondition(!chatModeSelected(noSelector))
precondition(chatModeSelected(noSelector,inheritedChat:true))
let legacyMarker = chatMarker("legacy-session","legacy-request")
let legacy = Binding(requestID:"legacy-request",sessionID:"legacy-session",marker:legacyMarker,
                     pid:42,launch:1,window:2,phase:"completed",sawStop:false,markerHistory:[legacyMarker])
precondition(provenChatBinding(legacy))
var wrongCapability = legacy
wrongCapability.capability = "image"
precondition(!provenChatBinding(wrongCapability))
let secondMarker = "[GeminiDesktop request second]"
var multi = nodes
multi.append(node("AXRow","","",1)) // 11: second user row
multi.append(node("AXRow","","",1)) // 12: second answer row
multi.append(node("AXHeading","userMessage.expandButton",secondMarker + " prompt",11))
multi.append(node("AXGroup","","",12))
multi.append(node("AXGroup","","",14))
multi.append(node("AXGroup","","",14))
multi.append(node("AXStaticText","","",15))
multi.append(node("AXButton","copy-button","复制回答",16))
let second = turn(makeUI(multi),marker:secondMarker,history:[marker,secondMarker],reader:{ $0 == 17 ? "second answer" : "" })
precondition(second.owned && decision(second,stop:false) == .completed && second.text == "second answer")
precondition(ownedAnswerRow(makeUI(multi),secondMarker,second.owned) == 12)
precondition(ownedAnswerRow(makeUI(multi),secondMarker,false) == nil)
var outputs = multi
outputs.append(node("AXImage","","生成的图片",15))
outputs.append(node("AXGroup","","正在生成视频…",15))
outputs.append(node("AXButton","","开始研究",15))
outputs.append(node("AXButton","","Canvas",15))
outputs.append(node("AXImage","","生成的图片",0)) // unrelated: never count
let candidates = candidateEvidence(outputs,12)
precondition(candidates == [CandidateEvidence(kind:"canvas",count:1),
    CandidateEvidence(kind:"generated_image",count:1),
    CandidateEvidence(kind:"research_confirmation",count:1),
    CandidateEvidence(kind:"video_generating",count:1)])
precondition(!turn(makeUI(multi),marker:secondMarker,history:[secondMarker],reader:read).owned)
precondition(!turn(makeUI(multi),marker:marker,history:[marker,secondMarker],reader:read).owned)
var manual = multi
manual.append(node("AXRow","","",1))
manual.append(node("AXHeading","userMessage.expandButton","manual followup",19))
precondition(!turn(makeUI(manual),marker:secondMarker,history:[marker,secondMarker],reader:read).owned)
precondition(hasForeignUserHeading(switchedUI, expected: [marker]))
precondition(hasForeignUserHeading(makeUI(manual), expected: [marker, secondMarker]))
precondition(!hasForeignUserHeading(blankGuard, expected: [marker]))
precondition(!hasForeignUserHeading(imageUI, expected: [marker]))
// 1.141 exposes user row headings without an identifier, and puts plain
// response text directly beside the action group rather than in a group.
let structural = [
    node("AXWindow","","",-1), node("AXGroup","","",0), node("AXRow","","",1),
    node("AXCell","","",2), node("AXGroup","","",3), node("AXHeading","",marker,4),
    node("AXRow","","",1), node("AXCell","","",6), node("AXGroup","","",7),
    node("AXStaticText","","",8), node("AXGroup","","",8),
    node("AXButton","copy-button","复制回答",10), node("AXTextArea","","问问 Gemini",0)
]
let structuralUI = UI(nodes:structural,input:12,stop:nil,newChat:nil,draft:"",dialog:false)
let structuralTurn = turn(structuralUI,marker:marker,reader: { $0 == 9 ? "answer" : "" })
precondition(structuralTurn.owned && structuralTurn.text == "answer" && decision(structuralTurn,stop:false) == .completed)
precondition(ownedAnswerRow(structuralUI,marker,structuralTurn.owned) == 6)
// Gemini 1.144.4 exposes answer prose blocks as AXButton beside actions and "来源" button.
let buttonProse = [
    node("AXWindow","","",-1), node("AXGroup","","",0), node("AXRow","","",1),
    node("AXCell","","",2), node("AXGroup","","",3), node("AXHeading","",marker,4),
    node("AXRow","","",1), node("AXCell","","",6), node("AXGroup","","",7),
    node("AXButton","","answer\n",8), node("AXGroup","","",8),
    node("AXButton","","来源",8), node("AXButton","copy-button","复制回答",10),
    node("AXTextArea","","问问 Gemini",0)
]
let buttonProseUI = UI(nodes:buttonProse,input:13,stop:nil,newChat:nil,draft:"",dialog:false)
let buttonProseTurn = turn(buttonProseUI,marker:marker,reader: { idx in
    if idx == 9 { return "answer\u{FFFC}\n\u{FFFC}" }
    return idx == 11 ? "来源" : ""
})
precondition(buttonProseTurn.owned && buttonProseTurn.copy && buttonProseTurn.text == "answer" && decision(buttonProseTurn,stop:false) == .completed)
var noCopy = nodes
noCopy[9].ident = "other-button"
t = turn(makeUI(noCopy),marker:marker,reader:read)
precondition(decision(t,stop:false) == .pending)
precondition(decision(turn(makeUI(nodes,"",true),marker:marker,reader:read),stop:true) == .waiting)
precondition(decision(turn(makeUI(echoed,"",true),marker:marker,reader:read),stop:true) == .waiting)
let limitTurn = turn(makeUI(nodes),marker:marker,reader:{ $0 == 8 ? String(repeating:"a",count:65536) : "" })
precondition(limitTurn.text?.utf8.count == 65536 && !limitTurn.tooLarge)
let largeTurn = turn(makeUI(nodes),marker:marker,reader:{ $0 == 8 ? String(repeating:"a",count:65537) : "" })
precondition(largeTurn.tooLarge && largeTurn.text == nil && decision(largeTurn,stop:false) == .tooLarge)
precondition(decision(largeTurn,stop:true) == .pending)
var consent = nodes
consent.append(node("AXGroup","","",6))
consent.append(node("AXButton","","允许一次",11))
consent.append(node("AXButton","","不允许",11))
precondition(decision(turn(makeUI(consent),marker:marker,reader:read),stop:true) == .waiting)
let dir = CommandLine.arguments[1]
precondition(safeDir(dir))
let f = path(dir,"request-id")
var b = Binding(requestID:"request-id",sessionID:"session",marker:marker,pid:42,launch:1,window:2,phase:"prepared",sawStop:false)
precondition(save(b,f,create:true) && !save(b,f,create:true))
precondition(load(f)?.phase == "prepared")
var cancelled = b
cancelled.phase = "cancelled"; cancelled.finalReason = "cancelled"; cancelled.finalCompletion = false
let cancelledReceipt = terminalReceipt(cancelled)
precondition(cancelledReceipt?.kind == "completed" && cancelledReceipt?.completion == false)
precondition(!mayMarkAccepted("cancel_sent"))
b.phase = "submitted"
precondition(save(b,f) && load(f)?.phase == "submitted")
var info = stat()
precondition(lstat(f,&info) == 0 && info.st_mode & 0o777 == 0o600)
let lf = dir + "/lease.json"
let lease = Lease(stateFile:f,requestID:b.requestID,sessionID:b.sessionID)
precondition(createLease(lease,at:lf) && !createLease(lease,at:lf))
precondition(leaseRelation(b,f,at:lf) == .own && leaseRelation(b,f + "wrong",at:lf) == .other)
precondition(matchesLease(b,f,at:lf) && !matchesLease(b,f + "wrong",at:lf))
precondition(!releaseLease(b,f + "wrong",at:lf) && matchesLease(b,f,at:lf))
precondition(releaseLease(b,f,at:lf) && !matchesLease(b,f,at:lf))
precondition(leaseRelation(b,f,at:lf) == .absent)
FileManager.default.createFile(atPath:lf,contents:Data("bad".utf8),attributes:[.posixPermissions:0o600])
precondition(leaseRelation(b,f,at:lf) == .unknown)
unlink(lf)
b.phase = "completed"; b.finalReason = "owned_response_action_observed"
b.finalText = "最终回答"; b.finalCompletion = true
precondition(save(b,f) && load(f)?.finalText == "最终回答")
// A generic chat receipt cannot complete Canvas or release its own lease.
var bogusCanvas = b
bogusCanvas.capability = "canvas"
let canvasReceipt = terminalReceipt(bogusCanvas)!
precondition(canvasReceipt.kind == "uncertain" && canvasReceipt.reason == "terminal_receipt_invalid")
precondition(createLease(lease,at:lf))
precondition(reconcileTerminalLease(bogusCanvas,f,canvasReceipt,at:lf) == "terminal_receipt_invalid")
precondition(matchesLease(bogusCanvas,f,at:lf))
var provenCanvas = bogusCanvas
provenCanvas.finalReason = "canvas_document_text"
provenCanvas.canvasBlankVerified = true; provenCanvas.canvasOpenAttempted = true; provenCanvas.canvasOpenPerformed = true
provenCanvas.finalCanvasTitle = "title"; provenCanvas.finalCanvasTextSHA = sha256Text(provenCanvas.finalText!)
provenCanvas.finalCanvasLocalID = canvasLocalID(provenCanvas.finalCanvasTitle!,provenCanvas.finalText!)
precondition(terminalReceipt(provenCanvas)?.completion == true)
precondition(canvasEvidence(provenCanvas)?["text_sha256"] as? String == provenCanvas.finalCanvasTextSHA)
var alteredCanvas = provenCanvas; alteredCanvas.finalText = "changed"
precondition(terminalReceipt(alteredCanvas)?.reason == "terminal_receipt_invalid")
// Identical continuation content is valid only after a proven close/reopen.
var sameCanvas = provenCanvas; sameCanvas.priorCanvasTextSHA = sameCanvas.finalCanvasTextSHA
precondition(terminalReceipt(sameCanvas)?.reason == "terminal_receipt_invalid")
sameCanvas.canvasCloseVerified = true
precondition(terminalReceipt(sameCanvas)?.completion == true)
// A correctly proven chat receipt still reconciles the matching lease.
precondition(reconcileTerminalLease(b,f,terminalReceipt(b)!,at:lf) == nil)
precondition(leaseRelation(b,f,at:lf) == .absent)
// Synthetic file validates the manifest gate only; it is not a native export.
let artifactsDir = URL(fileURLWithPath:dir).appendingPathComponent("artifacts",isDirectory:true)
try! FileManager.default.createDirectory(at:artifactsDir,withIntermediateDirectories:false,
                                         attributes:[.posixPermissions:0o700])
let imageURL = artifactsDir.appendingPathComponent("synthetic.png")
let cs = CGColorSpaceCreateDeviceRGB()
let context = CGContext(data:nil,width:2,height:2,bitsPerComponent:8,bytesPerRow:8,
                        space:cs,bitmapInfo:CGImageAlphaInfo.premultipliedLast.rawValue)!
context.setFillColor(CGColor(red:0,green:0,blue:1,alpha:1)); context.fill(CGRect(x:0,y:0,width:2,height:2))
let image = context.makeImage()!
let destination = CGImageDestinationCreateWithURL(imageURL as CFURL,"public.png" as CFString,1,nil)!
CGImageDestinationAddImage(destination,image,nil)
precondition(CGImageDestinationFinalize(destination))
chmod(imageURL.path,0o600)
let imageData = try! Data(contentsOf:imageURL)
let imageHash = SHA256.hash(data:imageData).map { String(format:"%02x",$0) }.joined()
let artifact = ImageArtifact(artifactID:"artifact-1",kind:"image",mimeType:"image/png",path:imageURL.path,
    byteLength:Int64(imageData.count),sha256:imageHash,requestID:"image-fixture",sessionID:"image-session")
var ib = Binding(requestID:"image-fixture",sessionID:"image-session",marker:marker,pid:42,launch:1,window:2,
                 phase:"completed",sawStop:false,capability:"image",finalReason:"native_image_export_verified",
                 finalCompletion:true,artifacts:[artifact])
precondition(verifiedArtifacts(ib,dir)?.count == 1)
// Even a valid file and claimed image-export reason cannot release a lease
// until a native-download provenance-producing flow exists.
let imageFile = path(dir,"image-fixture")
let imageLease = Lease(stateFile:imageFile,requestID:ib.requestID,sessionID:ib.sessionID)
let imageReceipt = terminalReceipt(ib)!
precondition(imageReceipt.kind == "uncertain" && imageReceipt.reason == "native_download_provenance_unverified")
precondition(createLease(imageLease,at:lf))
precondition(reconcileTerminalLease(ib,imageFile,imageReceipt,at:lf) == "native_download_provenance_unverified")
precondition(matchesLease(ib,imageFile,at:lf))
precondition(releaseLease(ib,imageFile,at:lf))
let outside = ImageArtifact(artifactID:"artifact-1",kind:"image",mimeType:"image/png",path:dir + "/outside.png",
    byteLength:Int64(imageData.count),sha256:imageHash,requestID:"image-fixture",sessionID:"image-session")
ib.artifacts = [outside]
precondition(verifiedArtifacts(ib,dir) == nil)
ib.artifacts = [artifact]
precondition(save(ib,path(dir,"image-fixture"),create:true))
// Clean markerless multi-turn, duplicate short prompts ("继续"), and truncated heading prefix matching.
var cleanNodes = [
    node("AXWindow", "", "", -1), node("AXGroup", "", "", 0), node("AXTextArea", "", "问问 Gemini", 0),
    node("AXRow", "", "", 1), node("AXRow", "", "", 1),
    node("AXHeading", "userMessage.expandButton", "记住暗号：春风十里", 3),
    node("AXGroup", "", "", 4), node("AXGroup", "", "", 6), node("AXGroup", "", "", 6),
    node("AXStaticText", "", "", 7), node("AXButton", "copy-button", "复制回答", 8),
    node("AXRow", "", "", 1), node("AXRow", "", "", 1),
    node("AXHeading", "userMessage.expandButton", "继续", 11),
    node("AXGroup", "", "", 12), node("AXGroup", "", "", 14), node("AXGroup", "", "", 14),
    node("AXStaticText", "", "", 15), node("AXButton", "copy-button", "复制回答", 16),
    node("AXRow", "", "", 1), node("AXRow", "", "", 1),
    node("AXHeading", "userMessage.expandButton", "继续", 19),
    node("AXGroup", "", "", 20), node("AXGroup", "", "", 22), node("AXGroup", "", "", 22),
    node("AXStaticText", "", "", 23), node("AXButton", "copy-button", "复制回答", 24),
]
cleanNodes.append(node("AXButton", "chat-input-mode-ask", "对话", 0))
let cleanUI = UI(nodes:cleanNodes,input:2,stop:nil,newChat:nil,draft:"",dialog:false)
let cleanHistory = ["记住暗号：春风十里", "继续", "继续"]
let cleanTurn = turn(cleanUI, marker:"继续", history:cleanHistory, reader:{ idx in
    if idx == 9 { return "第一轮回答" }
    if idx == 17 { return "第二轮回答" }
    if idx == 25 { return "第三轮回答" }
    return ""
})
precondition(cleanTurn.owned && cleanTurn.text == "第三轮回答" && decision(cleanTurn,stop:false) == .completed)
precondition(ownedAnswerRow(cleanUI, "继续", true) == 20)
precondition(chatSiblingRowCount(cleanUI) == 6)
let longPrompt = "请帮我分析一下为什么多轮对话会在第三个回合莫名其妙断开并且检查日志定位根本原因"
precondition(headingMatchesExpected("请帮我分析一下为什么多轮对话会在第三个回合…", longPrompt))
precondition(!headingMatchesExpected("完全不相关的其他对话", longPrompt))
var cleanPrior = Binding(requestID:"clean-3",sessionID:"session",marker:chatMarker("session","clean-3"),
                         pid:42,launch:1,window:2,phase:"completed",sawStop:false,
                         markerHistory:[chatMarker("session","clean-1"), chatMarker("session","clean-2"), chatMarker("session","clean-3")],
                         promptText:"继续",promptHistory:cleanHistory,capability:"chat",
                         finalReason:"owned_response_action_observed",finalText:"旧格式文本（含微调）",finalCompletion:true)
precondition(sendContextOK(cleanUI, "", cleanPrior, reader:{ idx in
    if idx == 25 { return "第三轮回答" }
    return ""
}, selectedReader:{ _ in true }))
print("ownership_lifecycle_fixtures_pass")
'''


class GeminiBridgeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        temp = pathlib.Path(cls.temp.name)
        cls.binary = str(temp / "gemini-bridge")
        subprocess.run(["/usr/bin/xcrun", "swiftc", str(SOURCE), "-o", cls.binary],
                       check=True, capture_output=True, timeout=90)
        fixture = temp / "fixture.swift"
        fixture.write_text(SOURCE.read_text().split("// BEGIN MAIN")[0] + FIXTURE)
        cls.fixture = str(temp / "fixture")
        subprocess.run(["/usr/bin/xcrun", "swiftc", str(fixture), "-o", cls.fixture],
                       check=True, capture_output=True, timeout=90)

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def call(self, command, value=None, raw=None):
        request = raw if raw is not None else json.dumps(value or {}, ensure_ascii=False).encode()
        result = subprocess.run([self.binary, command], input=request, capture_output=True, timeout=30)
        self.assertEqual(result.stderr, b"")
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 1)
        response = json.loads(lines[0])
        self.assertEqual(response["version"], 1)
        return result, response

    def test_owned_turn_and_state_machine(self):
        with tempfile.TemporaryDirectory() as d:
            result = subprocess.run([self.fixture, d], capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(result.stdout.strip(), b"ownership_lifecycle_fixtures_pass")
            request = {"request_id": "request-id", "session_id": "session",
                       "state_dir": d, "timeout_ms": 1000}
            for command in ("resume", "cancel"):
                result, response = self.call(command, request)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(response["kind"], "completed")
                self.assertTrue(response["completion_observed"])
                self.assertEqual(response["text"], "最终回答")
                self.assertEqual(response["text_format"], "plain_text")
            state_file = next(p for p in pathlib.Path(d).glob("gemini-*.json")
                              if json.loads(p.read_text())["requestID"] == "request-id")
            state = json.loads(state_file.read_text())
            state.update(phase="cancelled", finalReason="cancelled", finalCompletion=False)
            state.pop("finalText", None)
            state_file.write_text(json.dumps(state))
            result, response = self.call("resume", request)
            self.assertEqual(result.returncode, 0)
            self.assertEqual(response["reason"], "cancelled")
            self.assertFalse(response["completion_observed"])
            for reason in ("response_too_large", "native_process_gone_outcome_unknown"):
                state.update(phase="terminal_uncertain", finalReason=reason, finalCompletion=False)
                state.pop("finalText", None)
                state_file.write_text(json.dumps(state))
                result, response = self.call("resume", request)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(response["kind"], "uncertain")
                self.assertEqual(response["reason"], reason)
                self.assertFalse(response["completion_observed"])
                self.assertNotIn("text", response)
                self.assertNotIn("not_submitted", response)
            state.update(phase="not_submitted", finalReason="helper_exited_before_submit", finalCompletion=False)
            state_file.write_text(json.dumps(state))
            result, response = self.call("resume", request)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response["kind"], "error")
            self.assertFalse(response["completion_observed"])
            self.assertTrue(response["not_submitted"])
            self.assertEqual(response["request_id"], "request-id")
            self.assertEqual(response["session_id"], "session")
            state.update(phase="failed", capability="image", finalReason="native_error_response",
                         finalCompletion=False, finalFailure=True,
                         finalText="Normally I can help with things like this, but I don't seem to have access to that content. You can try again or ask me for something else.")
            state_file.write_text(json.dumps(state))
            result, response = self.call("resume", request)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response["kind"], "failed")
            self.assertEqual(response["reason"], "native_error_response")
            self.assertTrue(response["failure_observed"])
            self.assertFalse(response["completion_observed"])
            image_request = {"request_id":"image-fixture", "session_id":"image-session",
                             "state_dir":d, "timeout_ms":1000}
            result, response = self.call("collect", image_request)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response["reason"], "native_download_provenance_unverified")
            self.assertFalse(response["completion_observed"])
            self.assertNotIn("artifacts", response)
            image_path = pathlib.Path(d) / "artifacts" / "synthetic.png"
            image_path.write_bytes(b"tampered")
            result, response = self.call("collect", image_request)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response["reason"], "artifact_integrity_failed")

    def test_capabilities_never_claim_unverified_media(self):
        result, response = self.call("capabilities")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(response["capabilities"]["chat"]["implementation"], "implemented_unverified")
        for name in ("image", "video", "deep_research", "canvas"):
            self.assertEqual(response["capabilities"][name]["implementation"], "experimental_unverified")
            self.assertFalse(response["capabilities"][name]["live_verified"])
            self.assertIsNone(response["capabilities"][name]["account_available"])
            self.assertFalse(response["capabilities"][name]["selector_observed"])
            self.assertEqual(response["capabilities"][name]["hint_source"], "packaged_localization")
            self.assertIsInstance(response["capabilities"][name]["composer_placeholder_hints"], dict)

    def test_probe_shape(self):
        result, response = self.call("probe")
        self.assertEqual(result.returncode, 0)
        for key in ("available", "adapter_ready", "app_running", "accessibility_trusted", "app_installed", "busy"):
            self.assertIsInstance(response[key], bool)

    def test_rejects_malformed_and_oversized_input(self):
        for raw in (b"not json", b" " * 1048577):
            result, response = self.call("run", raw=raw)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response["reason"], "invalid_request")
            self.assertFalse(response["completion_observed"])
            self.assertNotIn("not_submitted", response)

    def test_timeout_ms_validation_and_64bit_boundary(self):
        with tempfile.TemporaryDirectory() as d:
            base = {
                "request_id": "timeout-check",
                "session_id": "dece3809-6e1f-47b4-aec0-0a746628aace",
                "prompt": "hello",
                "state_dir": d,
            }
            for bad_timeout in (-1, 50, 86_400_001, "1000", True, 1.5):
                req = dict(base, timeout_ms=bad_timeout)
                result, response = self.call("run", req)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(response["reason"], "invalid_request")
            # Valid 86_400_000 must pass timeout parsing (never fail with invalid_request)
            req = dict(base, timeout_ms=86_400_000, model="unknown-model-v99")
            result, response = self.call("run", req)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response["reason"], "unsupported_model_selection")

    def test_rejects_unknown_model_and_effort_without_exposing_prompt(self):
        request = {"request_id": "r1", "session_id": "dece3809-6e1f-47b4-aec0-0a746628aace", "prompt": "private 测试", "state_dir": self.temp.name,
                   "model": "unknown-model-v99", "timeout_ms": 1000}
        result, response = self.call("run", request)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(response["reason"], "unsupported_model_selection")
        self.assertNotIn(request["prompt"], json.dumps(response, ensure_ascii=False))

        bad_effort_req = dict(request, model="gemini-4-argon", effort="ultra-999")
        result, response = self.call("run", bad_effort_req)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(response["reason"], "unsupported_effort_selection")
        self.assertNotIn(request["prompt"], json.dumps(response, ensure_ascii=False))

    def test_unverified_media_and_attachments_rejected_before_mutation(self):
        request = {"request_id":"new", "session_id":"dece3809-6e1f-47b4-aec0-0a746628aace",
                   "prompt":"hello", "state_dir":self.temp.name, "timeout_ms":1000}
        for mode in ("unknown-mode",):
            result, response = self.call("run", dict(request, capability=mode))
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response["reason"], "capability_selection_unverified")
        result, response = self.call("run", dict(request, inputs=["attachment"]))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(response["reason"], "attachment_input_unverified")
        result, response = self.call("continue", dict(request, prior_request_id="old", artifact_context={"id":"1"}))
        self.assertEqual(response["reason"], "artifact_context_unverified")

    def test_resume_and_cancel_without_binding_fail_closed(self):
        with tempfile.TemporaryDirectory() as d:
            request = {"request_id": "r2", "session_id": "s2", "state_dir": d, "timeout_ms": 1000}
            for command in ("resume", "cancel", "inspect"):
                result, response = self.call(command, request)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(response["kind"], ("error", "uncertain"))
                self.assertFalse(response["completion_observed"])
                self.assertNotIn("text", response)
            self.assertEqual(os.listdir(d), [])


if __name__ == "__main__":
    unittest.main()
