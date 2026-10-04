import Foundation
import AppKit
import ApplicationServices
import Darwin
import CryptoKit
// Chat-row selection/action sequence follows OpenKakao ax_send (MIT). See third_party/openkakao.

let senderBase=URL(fileURLWithPath:CommandLine.arguments[0]).standardizedFileURL.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
let ipcIndex=CommandLine.arguments.firstIndex(of:"--ipc-dir")
let ipcOverride=ipcIndex.flatMap{CommandLine.arguments.indices.contains($0+1) ? CommandLine.arguments[$0+1] : nil}
let senderState=ipcOverride.map{URL(fileURLWithPath:$0)} ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/CommunicationHub/kakao-ipc")
var resolvedChatName:String?=nil
let startedAt=Date()
var timing:[String:Double]=[:]
var draftRestored=false
var textSent=false
var inputStarted=false
var attachmentSent=false
var cleanupClipboard:(()->Void)?=nil
var openTarget:[String:Any]?=nil
var attachmentVerification:String?=nil
var listRows:Int?=nil
var listedRooms:[[String:Any]]?=nil
func stamp(_ key:String){
 timing[key]=Date().timeIntervalSince(startedAt)
 if let request=requestKey {
  let dir=senderState.appendingPathComponent("request-status")
  try? FileManager.default.createDirectory(at:dir,withIntermediateDirectories:true,attributes:[.posixPermissions:0o700])
  let path=dir.appendingPathComponent(request+".json")
  try? JSONSerialization.data(withJSONObject:["pid":ProcessInfo.processInfo.processIdentifier,"phase":key,"at":Date().timeIntervalSince1970]).write(to:path,options:.atomic)
  try? FileManager.default.setAttributes([.posixPermissions:0o600],ofItemAtPath:path.path)
 }
}
let requestIndex=CommandLine.arguments.firstIndex(of:"--request")
let requestKey=requestIndex.flatMap{CommandLine.arguments.indices.contains($0+1) ? CommandLine.arguments[$0+1] : nil}
stamp("started")
// Kakao is raised to send. Left in front with the room open, it stops posting notifications for
// that room, so the next call there would never reach the hub; hand focus back when done.
let previousFrontmost=NSWorkspace.shared.frontmostApplication
func restoreFrontmost(){
 guard let previous=previousFrontmost,previous.bundleIdentifier != "com.kakao.KakaoTalkMac",!previous.isTerminated,
       NSWorkspace.shared.frontmostApplication?.bundleIdentifier=="com.kakao.KakaoTalkMac" else{return}
 _=previous.activate(options:[])
}
func finish(_ status:String,_ reason:String="") -> Never {
 cleanupClipboard?();cleanupClipboard=nil
 restoreFrontmost()
 var r:[String:Any]=["status":status,"reason":reason]
 if let name=resolvedChatName {r["verified_chat_name"]=name}
 if let target=openTarget {r["open_target"]=target}
 r["timing_seconds"]=timing
 r["draft_restored"]=draftRestored
 r["text_sent"]=textSent
 r["attachment_sent"]=attachmentSent
 if let check=attachmentVerification {r["attachment_verification"]=check}
 if let rows=listRows {r["list_rows"]=rows}
 if let rooms=listedRooms {r["rooms"]=rooms}
 r["elapsed_seconds"]=Date().timeIntervalSince(startedAt)
 if let key=requestKey,key.range(of:"^[0-9a-f]{64}$",options:.regularExpression) != nil {
 let output=senderState.appendingPathComponent("sender-receipts/"+key+".json")
 try? FileManager.default.createDirectory(at:output.deletingLastPathComponent(),withIntermediateDirectories:true,attributes:[.posixPermissions:0o700])
 try? JSONSerialization.data(withJSONObject:r).write(to:output,options:.atomic)
 try? FileManager.default.setAttributes([.posixPermissions:0o600],ofItemAtPath:output.path)
 }
 FileHandle.standardOutput.write((try! JSONSerialization.data(withJSONObject:r))+Data([10]))
 exit(status=="sent_verified" || status=="ready" ? 0 : 2)
}
func attr(_ e:AXUIElement,_ key:String)->CFTypeRef? {var v:CFTypeRef?;guard AXUIElementCopyAttributeValue(e,key as CFString,&v) == .success else{return nil};return v}
func str(_ e:AXUIElement,_ key:String)->String {attr(e,key) as? String ?? ""}
func children(_ e:AXUIElement)->[AXUIElement] {attr(e,kAXChildrenAttribute) as? [AXUIElement] ?? []}
struct Node {
 let element:AXUIElement
 let role:String;let value:String;let title:String;let description:String;let placeholder:String;let enabled:Bool
 var isComposer:Bool {role==kAXTextAreaRole && (description=="메시지 입력" || title=="메시지 입력" || placeholder=="메시지 입력")}
 var isSend:Bool {role==kAXButtonRole && (title=="전송" || description=="전송")}
}
// visibleRowsOnly: walk only on-screen table rows. Used where the target is known to be on screen
// (a just-sent message, an upload sheet); full history traversal stays the default.
func collect(_ root:AXUIElement,visibleRowsOnly:Bool=false)->[Node] {
 var result:[Node]=[]
 let optimizeTableRows=visibleRowsOnly || str(root,kAXTitleAttribute)=="카카오톡"
 let keys=[kAXRoleAttribute,kAXValueAttribute,kAXTitleAttribute,kAXDescriptionAttribute,"AXPlaceholderValue",kAXEnabledAttribute,kAXChildrenAttribute] as CFArray
 func visit(_ e:AXUIElement,_ depth:Int){
  if depth>24 || result.count>=8000{return}
  var output:CFArray?
  guard AXUIElementCopyMultipleAttributeValues(e,keys,AXCopyMultipleAttributeOptions(rawValue:0),&output) == .success,let a=output as? [Any],a.count==7 else{return}
  result.append(Node(element:e,role:a[0] as? String ?? "",value:a[1] as? String ?? "",title:a[2] as? String ?? "",description:a[3] as? String ?? "",placeholder:a[4] as? String ?? "",enabled:a[5] as? Bool ?? false))
  let descendants:[AXUIElement]
  if optimizeTableRows,a[0] as? String == kAXTableRole,let visible=attr(e,"AXVisibleRows") as? [AXUIElement],!visible.isEmpty {descendants=visible}
  else {descendants=a[6] as? [AXUIElement] ?? []}
  for child in descendants {visit(child,depth+1)}
 }
 visit(root,0);return result
}
// A just-received trigger is almost always on screen. Visible rows are a subset of the full
// history, so finding it there implies the full walk would too; only a miss pays for the full walk.
func collectWithTrigger(_ window:AXUIElement,_ trigger:String)->[Node] {
 let visible=collect(window,visibleRowsOnly:true)
 return visible.contains(where:{$0.value==trigger}) ? visible : collect(window)
}
guard AXIsProcessTrusted() else{finish("held","accessibility_permission_required")}
if CommandLine.arguments.contains("--check"){finish("ready")}
try FileManager.default.createDirectory(at:senderState,withIntermediateDirectories:true,attributes:[.posixPermissions:0o700])
let uiLock=open(senderState.appendingPathComponent("ui-send.lock").path,O_CREAT|O_RDWR,0o600)
guard uiLock>=0,flock(uiLock,LOCK_EX|LOCK_NB)==0 else{finish("held","sender_busy")}
let raw:Data
if let key=requestKey {
 guard key.range(of:"^[0-9a-f]{64}$",options:.regularExpression) != nil else{finish("held","invalid_request_key")}
 guard let data=try? Data(contentsOf:senderState.appendingPathComponent("sender-requests/"+key+".json")) else{finish("held","missing_request")}
 raw=data
} else {raw=FileHandle.standardInput.readDataToEndOfFile()}
guard let p=(try? JSONSerialization.jsonObject(with:raw)) as? [String:Any],let name=p["chat_name"] as? String,let trigger=p["trigger_body"] as? String,let reply=p["reply"] as? String,!name.isEmpty,!trigger.isEmpty,(reply.hasPrefix("[System-유이] : ") || reply.hasPrefix("[System-유미] : ")),reply.count<8192 else{finish("held","invalid_input")}
let phase=p["phase"] as? String ?? "combined"
guard phase=="combined" || phase=="attachment_only" else{finish("held","invalid_delivery_phase")}
if phase=="attachment_only" && p["prior_text_verified"] as? Bool != true {finish("held","prior_text_verification_missing")}
stamp("input_read")
func requireUnexpired(){
 if let deadline=p["expires_at"] as? Double,Date().timeIntervalSince1970>deadline{finish(textSent ? "partial_file_held":(inputStarted ? "sending":"held"),"request_expired")}
}
requireUnexpired()
if let session=CGSessionCopyCurrentDictionary() as? [String:Any],session["CGSSessionScreenIsLocked"] as? Bool == true {finish("held","screen_locked")}
let apps=NSRunningApplication.runningApplications(withBundleIdentifier:"com.kakao.KakaoTalkMac")
guard apps.count==1 else{finish("held","kakao_not_running_or_ambiguous")}
let app=apps[0],root=AXUIElementCreateApplication(app.processIdentifier)
func postKeyboardKey(_ keyCode:CGKeyCode,_ flags:CGEventFlags=[]) -> Bool {
 guard let down=CGEvent(keyboardEventSource:nil,virtualKey:keyCode,keyDown:true),let up=CGEvent(keyboardEventSource:nil,virtualKey:keyCode,keyDown:false) else{return false}
 down.flags=flags;up.flags=flags
 down.postToPid(app.processIdentifier);Thread.sleep(forTimeInterval:0.03);up.postToPid(app.processIdentifier)
 return true
}
func windowList()->[AXUIElement] {attr(root,kAXWindowsAttribute) as? [AXUIElement] ?? []}
if p["probe"] as? Bool == true && p["inspect_windows"] as? Bool == true {
 openTarget=["window_titles":windowList().map{str($0,kAXTitleAttribute)}]
 finish("ready")
}
// Kakao advertises Cmd+2 for its chat list. Address only its process, never
// the user's global keyboard, and verify the resulting main window.
if !windowList().contains(where:{str($0,kAXTitleAttribute)=="카카오톡"}) {
 _=app.activate(options:[])
 if let down=CGEvent(keyboardEventSource:nil,virtualKey:19,keyDown:true),let up=CGEvent(keyboardEventSource:nil,virtualKey:19,keyDown:false){
  down.flags = .maskCommand;up.flags = .maskCommand
  down.postToPid(app.processIdentifier);Thread.sleep(forTimeInterval:0.03);up.postToPid(app.processIdentifier)
 }
 let deadline=Date().addingTimeInterval(1)
 while Date()<deadline && !windowList().contains(where:{str($0,kAXTitleAttribute)=="카카오톡"}){Thread.sleep(forTimeInterval:0.05)}
}
func labelMatches(_ text:String,_ expected:String)->Bool {
 let compact=text.split(whereSeparator:{$0.isWhitespace}).joined(separator:" ")
 if compact==expected{return true}
 let pattern="^"+NSRegularExpression.escapedPattern(for:expected)+"(?:\\s+\\d+)?\\s+(?:(?:오전|오후)\\s+\\d{1,2}:\\d{2}|\\d{1,2}월\\s+\\d{1,2}일|어제|오늘)(?:\\s+\\d+)?$"
 return compact.range(of:pattern,options:.regularExpression) != nil
}
func findCandidates()->[(AXUIElement,[Node])] {
 let wins=windowList(),named=wins.filter{str($0,kAXTitleAttribute)==name}
 if named.count>1 {finish("held","duplicate_named_windows")}
 let scope=(p["room_name_verified"] as? Bool == true) ? named : wins
 var found:[(AXUIElement,[Node])]=[]
 for window in scope {
  let tree=collectWithTrigger(window,trigger)
  if tree.contains(where:{$0.value==trigger}) && tree.contains(where:{$0.isComposer}) {found.append((window,tree))}
 }
 return found
}
let roomNameVerified=p["room_name_verified"] as? Bool == true
let triggerDigest=SHA256.hash(data:Data(trigger.utf8)).map{String(format:"%02x",$0)}.joined()
let listScanURL=senderState.appendingPathComponent("list-scan.json")
var openedFromList=false
// The full per-row scan proves the name is unique in the chat list. A probe or ACK for the same
// event runs it moments earlier, so a send that found the room already open may reuse that proof
// when the name, trigger, list size and strictness all match within 90 seconds.
func reusableListScan(_ actualName:String,_ rowCount:Int)->Bool {
 guard !openedFromList,let data=try? Data(contentsOf:listScanURL),
       let c=(try? JSONSerialization.jsonObject(with:data)) as? [String:Any],
       c["chat_name"] as? String==actualName,c["trigger_sha256"] as? String==triggerDigest,c["rows"] as? Int==rowCount,
       c["room_name_verified"] as? Bool==false || c["room_name_verified"] as? Bool==roomNameVerified,
       let at=c["verified_at"] as? Double else{return false}
 let age=Date().timeIntervalSince1970-at
 return age>=0 && age<=90
}
func verifyListTarget(_ actualName:String){
 let main=windowList().filter{str($0,kAXTitleAttribute)=="카카오톡"}
 guard main.count==1 else{finish("held","chat_list_window_missing_or_ambiguous")}
 let tables=collect(main[0]).filter{$0.role==kAXTableRole}
 guard tables.count==1,let rows=attr(tables[0].element,"AXRows") as? [AXUIElement],!rows.isEmpty,rows.count<=10000 else{finish("held","complete_chat_rows_unavailable")}
 if let count=attr(tables[0].element,"AXRowCount") as? Int,count>rows.count {finish("held","chat_rows_incomplete")}
 listRows=rows.count
 // An owner-approved room was proven unique at this list size; unchanged size means no room was
 // added or removed since, so the per-call scan is skipped.
 if let expected=p["room_verified_rows"] as? Int,expected==rows.count {stamp("room_registry_verified");return}
 if reusableListScan(actualName,rows.count) {stamp("list_scan_reused");return}
 let matching=rows.filter{row in collect(row).contains{node in node.role==kAXStaticTextRole && [node.value,node.title,node.description].contains{labelMatches($0,actualName)}}}
 guard matching.count==1 else{finish("held","duplicate_or_missing_chat_name")}
 if p["room_name_verified"] as? Bool != true {
  let previews=rows.filter{row in collect(row).contains{node in node.role==kAXTextAreaRole && [node.value,node.title,node.description].contains(trigger)}}
  guard previews.count==1,CFEqual(previews[0],matching[0]) else{finish("held","unknown_room_preview_ambiguous_or_changed")}
 }
 let scan:[String:Any]=["chat_name":actualName,"trigger_sha256":triggerDigest,"room_name_verified":roomNameVerified,"rows":rows.count,"verified_at":Date().timeIntervalSince1970]
 if let data=try? JSONSerialization.data(withJSONObject:scan) {
  try? data.write(to:listScanURL,options:.atomic)
  try? FileManager.default.setAttributes([.posixPermissions:0o600],ofItemAtPath:listScanURL.path)
 }
 stamp("list_scanned")
}
// Registration check from the dashboard: prove the name is unique in the chat list without
// opening the room or writing anything, and report the list size it was proven at.
// Dashboard picker: each chat-list row's name (its first static text), read only on the owner's
// request. Previews, times and counts are not returned.
if p["list_rooms"] as? Bool == true {
 let main=windowList().filter{str($0,kAXTitleAttribute)=="카카오톡"}
 guard main.count==1 else{finish("held","chat_list_window_missing_or_ambiguous")}
 let tables=collect(main[0]).filter{$0.role==kAXTableRole}
 guard tables.count==1,let rows=attr(tables[0].element,"AXRows") as? [AXUIElement],!rows.isEmpty,rows.count<=10000 else{finish("held","complete_chat_rows_unavailable")}
 listedRooms=rows.map{row in
  ["name":collect(row).filter{$0.role==kAXStaticTextRole}.flatMap{[$0.value,$0.title,$0.description]}.first{!$0.isEmpty} ?? ""]
 }
 listRows=rows.count
 finish("ready","rooms_listed")
}
if p["verify_room"] as? Bool == true {
 let main=windowList().filter{str($0,kAXTitleAttribute)=="카카오톡"}
 guard main.count==1 else{finish("held","chat_list_window_missing_or_ambiguous")}
 let tables=collect(main[0]).filter{$0.role==kAXTableRole}
 guard tables.count==1,let rows=attr(tables[0].element,"AXRows") as? [AXUIElement],!rows.isEmpty,rows.count<=10000 else{finish("held","complete_chat_rows_unavailable")}
 if let count=attr(tables[0].element,"AXRowCount") as? Int,count>rows.count {finish("held","chat_rows_incomplete")}
 let matching=rows.filter{row in collect(row).contains{node in node.role==kAXStaticTextRole && [node.value,node.title,node.description].contains{labelMatches($0,name)}}}
 guard matching.count==1 else{finish("held","duplicate_or_missing_chat_name")}
 listRows=rows.count;resolvedChatName=name
 finish("ready","room_name_unique")
}
if p["probe"] as? Bool == true && p["close_probe"] as? Bool == true {
 let target=windowList().filter{str($0,kAXTitleAttribute)==name}
 guard target.count==1 else{finish("held","close_probe_target_ambiguous")}
 let editors=collect(target[0]).filter{$0.isComposer}
 guard editors.count==1 && editors[0].value.isEmpty else{finish("held","close_probe_draft_present")}
 guard let button=attr(target[0],kAXCloseButtonAttribute),CFGetTypeID(button)==AXUIElementGetTypeID() else{finish("held","close_probe_button_missing")}
 _=AXUIElementPerformAction(button as! AXUIElement,kAXPressAction as CFString)
 let deadline=Date().addingTimeInterval(2)
 while Date()<deadline {
  if !windowList().contains(where:{str($0,kAXTitleAttribute)==name}){finish("ready","target_closed_verified")}
  Thread.sleep(forTimeInterval:0.05)
 }
 finish("held","close_probe_not_verified")
}
var candidates=findCandidates()
if candidates.isEmpty {
 openedFromList=true
 let main=windowList().filter{str($0,kAXTitleAttribute)=="카카오톡"}
 guard main.count==1 else{finish("held","chat_list_window_missing_or_ambiguous")}
 let rows=collect(main[0]).filter{$0.role==kAXRowRole}
 let matching=rows.filter{row in
  let content=collect(row.element)
  if p["room_name_verified"] as? Bool == true {
   return content.contains{node in
    node.role==kAXStaticTextRole && [node.value,node.title,node.description].contains{labelMatches($0,name)}
   }
  }
  return content.contains{node in
   node.role==kAXTextAreaRole && [node.value,node.title,node.description].contains(trigger)
  }
 }
 guard matching.count==1 else{finish("held","chat_list_target_missing_or_ambiguous")}
 let row=matching[0].element
 let tables=collect(main[0]).filter{$0.role==kAXTableRole}
 guard tables.count==1 else{finish("held","chat_list_table_ambiguous")}
 let table=tables[0].element
 guard AXUIElementSetAttributeValue(table,"AXSelectedRows" as CFString,[row] as CFArray) == .success else{finish("held","chat_row_selection_failed")}
 guard let selected=attr(table,"AXSelectedRows") as? [AXUIElement],selected.count==1,CFEqual(selected[0],row) else{finish("held","chat_row_selection_not_verified")}
 _=AXUIElementSetAttributeValue(table,kAXFocusedAttribute as CFString,kCFBooleanTrue)
 func actions(_ element:AXUIElement)->[String] {
  var names:CFArray?;guard AXUIElementCopyActionNames(element,&names) == .success else{return []}
  return names as? [String] ?? []
 }
 let rowActions=actions(row),tableActions=actions(table)
 let opened:AXError
 if rowActions.contains(kAXPressAction) {opened=AXUIElementPerformAction(row,kAXPressAction as CFString)}
 else if rowActions.contains("AXConfirm") {opened=AXUIElementPerformAction(row,"AXConfirm" as CFString)}
 else if tableActions.contains("AXConfirm") {opened=AXUIElementPerformAction(table,"AXConfirm" as CFString)}
 else{
  // AX supplies and rechecks the target for a pointer-free open;
  // no screenshot, external desktop server or untrusted command is involved.
  _=app.activate(options:[])
  guard AXUIElementPerformAction(main[0],kAXRaiseAction as CFString) == .success else{finish("held","chat_list_raise_failed")}
  _=AXUIElementSetAttributeValue(main[0],kAXMainAttribute as CFString,kCFBooleanTrue)
  let focusDeadline=Date().addingTimeInterval(1)
  while Date()<focusDeadline {
   if let focused=attr(root,kAXFocusedWindowAttribute),CFGetTypeID(focused)==AXUIElementGetTypeID(),CFEqual(focused,main[0]),NSWorkspace.shared.frontmostApplication?.processIdentifier==app.processIdentifier {break}
   Thread.sleep(forTimeInterval:0.03)
  }
  guard let focused=attr(root,kAXFocusedWindowAttribute),CFGetTypeID(focused)==AXUIElementGetTypeID(),CFEqual(focused,main[0]),NSWorkspace.shared.frontmostApplication?.processIdentifier==app.processIdentifier else{finish("held","chat_list_focus_not_verified")}
  // AX focus can precede the WindowServer's completed front-window transition.
  Thread.sleep(forTimeInterval:0.2)
  guard let selectedNow=attr(table,"AXSelectedRows") as? [AXUIElement],selectedNow.count==1,CFEqual(selectedNow[0],row) else{finish("held","chat_row_selection_changed")}
  let freshRow=collect(row)
  let stillMatches=freshRow.contains{node in
   if p["room_name_verified"] as? Bool == true {return node.role==kAXStaticTextRole && [node.value,node.title,node.description].contains{labelMatches($0,name)}}
   return node.role==kAXTextAreaRole && [node.value,node.title,node.description].contains(trigger)
  }
  guard stillMatches else{finish("held","chat_row_content_changed")}
  // Open only via a key addressed to Kakao's verified chat-list focus.
  // Never post mouse events or send a global keyboard event.
  guard AXUIElementSetAttributeValue(table,kAXFocusedAttribute as CFString,kCFBooleanTrue) == .success,
        let focusedElement=attr(root,kAXFocusedUIElementAttribute),CFGetTypeID(focusedElement)==AXUIElementGetTypeID(),
        (CFEqual(focusedElement,table) || freshRow.contains(where:{CFEqual($0.element,focusedElement)})) else{finish("held","chat_list_keyboard_focus_not_verified")}
  let pointerBefore=CGEvent(source:nil)?.location
  guard let down=CGEvent(keyboardEventSource:nil,virtualKey:36,keyDown:true),let up=CGEvent(keyboardEventSource:nil,virtualKey:36,keyDown:false) else{finish("held","keyboard_event_creation_failed")}
  down.postToPid(app.processIdentifier);Thread.sleep(forTimeInterval:0.03);up.postToPid(app.processIdentifier)
  stamp("scoped_keyboard_open_completed")
  let pointerAfter=CGEvent(source:nil)?.location
  openTarget=["transport":"pid_keyboard","mouse_events_posted":false,"pointer_same_at_observation":pointerBefore==pointerAfter]
  opened = .success
 }
 guard opened == .success else{finish("held","chat_row_open_action_failed")}
 let deadline=Date().addingTimeInterval(4)
 while Date()<deadline {
  candidates=findCandidates()
  if !candidates.isEmpty{break}
  Thread.sleep(forTimeInterval:0.1)
 }
 stamp("chat_opened")
}
guard candidates.count==1 else{finish("held","trigger_chat_window_missing_or_ambiguous")}
let (win,nodes)=candidates[0]
let actualName=str(win,kAXTitleAttribute)
guard !actualName.isEmpty else{finish("held","missing_actual_chat_name")}
resolvedChatName=actualName
verifyListTarget(actualName)
stamp("target_verified")
if p["probe"] as? Bool == true {finish("ready")}
let editors=nodes.filter{$0.isComposer}
guard editors.count==1 else{finish("held","composer_ambiguous")}
let editor=editors[0].element
let originalDraft=str(editor,kAXValueAttribute)
func sameVerifiedRoom(_ expectedDraft:String)->Bool {
 let named=windowList().filter{str($0,kAXTitleAttribute)==actualName}
 guard named.count==1,CFEqual(named[0],win),str(win,kAXTitleAttribute)==actualName else{return false}
 let fresh=collectWithTrigger(win,trigger)
 guard fresh.contains(where:{$0.value==trigger}) else{return false}
 let composers=fresh.filter{$0.isComposer}
 guard composers.count==1,CFEqual(composers[0].element,editor),composers[0].value==expectedDraft else{return false}
 // Keys are PID-scoped and each key operation verifies the exact AX focused element.
 // The globally foreground app may be unrelated; it is not a routing authority.
 return true
}
var backupURL:URL?=nil
if !originalDraft.isEmpty {
 let dir=senderState.appendingPathComponent("draft-backups")
 do {
  try FileManager.default.createDirectory(at:dir,withIntermediateDirectories:true,attributes:[.posixPermissions:0o700])
  let url=dir.appendingPathComponent((requestKey ?? UUID().uuidString)+".json")
  try JSONSerialization.data(withJSONObject:["chat_name":actualName,"draft":originalDraft,"captured_at":Date().timeIntervalSince1970]).write(to:url,options:.atomic)
  try FileManager.default.setAttributes([.posixPermissions:0o600],ofItemAtPath:url.path)
  backupURL=url
 } catch {finish("held","draft_backup_failed")}
}
func restoreDraft(){
 // Never replace text typed by the owner while delivery was being verified.
 if str(win,kAXTitleAttribute)==actualName && str(editor,kAXValueAttribute).isEmpty {
  if originalDraft.isEmpty {draftRestored=true}
  else if AXUIElementSetAttributeValue(editor,kAXValueAttribute as CFString,originalDraft as CFString) == .success && str(editor,kAXValueAttribute)==originalDraft {draftRestored=true}
 }
 if draftRestored,let url=backupURL {try? FileManager.default.removeItem(at:url)}
}
func performAttachment(_ path:String){
 requireUnexpired()
 let file=URL(fileURLWithPath:path).standardizedFileURL.resolvingSymlinksInPath()
 let allowed=senderState.appendingPathComponent("file-jobs").standardizedFileURL.path+"/"
 let suffix=file.pathExtension.lowercased()
 guard file.path.hasPrefix(allowed),FileManager.default.fileExists(atPath:file.path),
       let header=try? Data(contentsOf:file,options:.mappedIfSafe),!header.isEmpty else{finish("partial_file_held","invalid_attachment_path")}
 let kind:String
 if suffix=="zip",header.starts(with:[0x50,0x4b,0x03,0x04]) {kind="zip"}
 else if suffix=="png",header.starts(with:[0x89,0x50,0x4e,0x47,0x0d,0x0a,0x1a,0x0a]) {kind="image"}
 else if suffix=="gif" && (header.prefix(6)==Data("GIF87a".utf8) || header.prefix(6)==Data("GIF89a".utf8)) {kind="image"}
 else {finish("partial_file_held","attachment_type_or_signature_unsupported")}
 if let session=CGSessionCopyCurrentDictionary() as? [String:Any],session["CGSSessionScreenIsLocked"] as? Bool == true {finish("partial_file_held","screen_locked")}
 guard sameVerifiedRoom(originalDraft) else{finish("partial_file_held","target_changed_before_attachment")}
 func filenameMarkerCount()->Int {
  collect(win).filter{node in
   let text=node.value+node.title+node.description
   guard text.contains(file.lastPathComponent) else{return false}
   if kind=="zip" {return node.role==kAXStaticTextRole && text.contains("유효기간")}
   return node.role==kAXStaticTextRole || node.role==kAXImageRole
  }.count
 }
 func uploadPreviewVisible()->Bool {
  let sheets=attr(win,"AXSheets") as? [AXUIElement] ?? []
  let tree=(sheets.isEmpty ? [win] : sheets).flatMap{collect($0,visibleRowsOnly:true)}
  let hasName=tree.contains{($0.value+$0.title+$0.description).contains(file.lastPathComponent)}
  let hasSend=tree.contains{$0.role==kAXButtonRole && ($0.title=="1개 전송" || $0.description=="1개 전송")}
  return hasName && hasSend
 }
 let beforeMarkers=kind=="zip" ? filenameMarkerCount():0
 let clipboard=NSPasteboard.general
 let saved=clipboard.pasteboardItems?.map{item in
  item.types.reduce(into:[NSPasteboard.PasteboardType:Data]()){result,type in
   if let data=item.data(forType:type){result[type]=data}
  }
 } ?? []
 clipboard.clearContents()
 guard clipboard.writeObjects([file as NSURL]) else{finish("partial_file_held","file_clipboard_failed")}
 let ownClipboardVersion=clipboard.changeCount
 cleanupClipboard = {
  if clipboard.changeCount==ownClipboardVersion {
   clipboard.clearContents()
   let items=saved.map{values -> NSPasteboardItem in
    let item=NSPasteboardItem();for (type,data) in values{item.setData(data,forType:type)};return item
   }
   clipboard.writeObjects(items)
  }
 }
 defer {cleanupClipboard?();cleanupClipboard=nil}
 _=AXUIElementPerformAction(win,kAXRaiseAction as CFString)
 _=app.activate(options:[])
 _=AXUIElementSetAttributeValue(editor,kAXFocusedAttribute as CFString,kCFBooleanTrue)
 guard let focused=attr(root,kAXFocusedUIElementAttribute),CFEqual(focused,editor),sameVerifiedRoom(originalDraft) else{finish("partial_file_held","attachment_focus_not_verified")}
 guard postKeyboardKey(9,.maskCommand) else{finish("partial_file_held","attachment_paste_key_creation_failed")}
 let previewDeadline=Date().addingTimeInterval(8)
 var uploadButton:AXUIElement?=nil
 while Date()<previewDeadline {
  Thread.sleep(forTimeInterval:0.1)
  let sheets=attr(win,"AXSheets") as? [AXUIElement] ?? []
  let previewNodes:[Node]=sheets.isEmpty ? collect(win,visibleRowsOnly:true) : sheets.flatMap{collect($0)}
  if previewNodes.contains(where:{($0.value+$0.title+$0.description).contains(file.lastPathComponent)}) {
   let sends=previewNodes.filter{$0.role==kAXButtonRole && ($0.title=="1개 전송" || $0.description=="1개 전송") && $0.enabled}
   if sends.count==1 {uploadButton=sends[0].element;break}
  }
 }
 guard let upload=uploadButton,str(win,kAXTitleAttribute)==actualName,sameVerifiedRoom(originalDraft) else{finish("partial_file_held","attachment_preview_not_verified")}
 guard AXUIElementSetAttributeValue(upload,kAXFocusedAttribute as CFString,kCFBooleanTrue) == .success,
       let sendFocused=attr(root,kAXFocusedUIElementAttribute),CFEqual(sendFocused,upload),
       str(win,kAXTitleAttribute)==actualName,sameVerifiedRoom(originalDraft) else{finish("partial_file_held","attachment_enter_focus_not_verified")}
 stamp("before_attachment_send")
 requireUnexpired()
 guard postKeyboardKey(36) else{finish("sending","attachment_enter_outcome_uncertain")}
 if kind=="image" {
  // Kakao image bubbles expose no filename and bubble geometry proved unreliable. Enter was
  // pressed on the verified "1개 전송" button of this room, so the upload sheet closing while
  // the same window stays in place is the bounded completion signal.
  let imageDeadline=Date().addingTimeInterval(6)
  while Date()<imageDeadline {
   Thread.sleep(forTimeInterval:0.1)
   let named=windowList().filter{str($0,kAXTitleAttribute)==actualName}
   if named.count==1 && CFEqual(named[0],win) && !uploadPreviewVisible() {
    attachmentSent=true;attachmentVerification="upload_sheet_closed";stamp("attachment_verified");return
   }
  }
  finish("sending","attachment_delivery_not_observed")
 }
 let deadline=Date().addingTimeInterval(35)
 while Date()<deadline {
  Thread.sleep(forTimeInterval:0.15)
  if str(win,kAXTitleAttribute)==actualName && sameVerifiedRoom(originalDraft) && !uploadPreviewVisible() && filenameMarkerCount()>beforeMarkers {
   attachmentSent=true;stamp("attachment_verified");return
  }
 }
 finish("sending","attachment_delivery_not_observed")
}
if phase=="attachment_only" {
 guard p["prior_text_verified"] as? Bool == true,str(editor,kAXValueAttribute).isEmpty,
       collect(win).contains(where:{$0.value==reply && !$0.isComposer}),sameVerifiedRoom("") else{finish("partial_file_held","prior_text_not_confirmed_in_target_room")}
 textSent=true;stamp("prior_text_verified_for_attachment")
 guard let path=p["attachment_path"] as? String else{finish("partial_file_held","missing_sticker_attachment_path")}
 performAttachment(path)
 finish("sent_verified","")
}
_=AXUIElementPerformAction(win,kAXRaiseAction as CFString)
_=app.activate(options:[])
guard sameVerifiedRoom(originalDraft) else{finish("held","target_or_draft_changed_before_write")}
requireUnexpired()
stamp("before_input")
inputStarted=true
guard AXUIElementSetAttributeValue(editor,kAXValueAttribute as CFString,reply as CFString) == .success else{finish("sending","composer_write_outcome_uncertain")}
guard str(win,kAXTitleAttribute)==actualName,str(editor,kAXValueAttribute)==reply else{finish("sending","composer_changed_after_write")}
guard AXUIElementSetAttributeValue(editor,kAXFocusedAttribute as CFString,kCFBooleanTrue) == .success,
      let textFocused=attr(root,kAXFocusedUIElementAttribute),CFEqual(textFocused,editor),
      str(editor,kAXValueAttribute)==reply,sameVerifiedRoom(reply) else{finish("sending","composer_focus_or_target_changed")}
// Own sends scroll the room to the bottom, so the new bubble is among the visible rows; counting
// only those keeps each check cheap while Kakao is busy right after Enter.
func visibleReplyCount()->Int {collect(win,visibleRowsOnly:true).filter{$0.value==reply && !$0.isComposer}.count}
let before=visibleReplyCount()
stamp("before_send")
requireUnexpired()
guard postKeyboardKey(36) else{finish("sending","text_enter_event_creation_failed")}
let sentDeadline=Date().addingTimeInterval(8)
while Date()<sentDeadline {
 Thread.sleep(forTimeInterval:0.05)
 if str(win,kAXTitleAttribute)==actualName && str(editor,kAXValueAttribute).isEmpty {
  if visibleReplyCount()>before {
   textSent=true;stamp("sent_verified");restoreDraft()
   if let path=p["attachment_path"] as? String {performAttachment(path)}
   finish("sent_verified",draftRestored ? "":"draft_saved_for_recovery")
  }
 }
}
finish("sending","text_enter_delivery_not_observed")
