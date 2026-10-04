// Kakao-scoped Notification Center conventions adapted from OpenKakao (MIT).
// No login, credential extraction, AX control, notification writes or chat DB access.
import AppKit
import Foundation
import SQLite3
import Darwin

let executable = URL(fileURLWithPath: CommandLine.arguments[0]).standardizedFileURL
let base = executable.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
let stateIndex=CommandLine.arguments.firstIndex(of:"--state-dir")
let stateOverride=stateIndex.flatMap{CommandLine.arguments.indices.contains($0+1) ? CommandLine.arguments[$0+1] : nil}
let state = stateOverride.map{URL(fileURLWithPath:$0)} ?? base.appendingPathComponent(".state/live")
try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
let database = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Group Containers/group.com.apple.usernoted/db2/db")
let doctor = CommandLine.arguments.contains("--doctor")
let hubIndex=CommandLine.arguments.firstIndex(of:"--hub-socket")
let hubSocket=hubIndex.flatMap{CommandLine.arguments.indices.contains($0+1) ? CommandLine.arguments[$0+1] : nil} ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/CommunicationHub/hub.sock").path
let receiverLock=doctor ? -1 : open(state.appendingPathComponent("receiver.lock").path,O_CREAT|O_RDWR,0o600)
if !doctor && (receiverLock<0 || flock(receiverLock,LOCK_EX|LOCK_NB) != 0){exit(0)}
@Sendable func sendToHub(_ item:[String:Any],_ socketPath:String)->Bool {
    guard socketPath.hasPrefix("/"),socketPath.utf8.count<104 else{return false}
    let fd=socket(AF_UNIX,SOCK_STREAM,0);guard fd>=0 else{return false};defer{close(fd)}
    var timeout=timeval(tv_sec:1,tv_usec:0),one:Int32=1
    _=setsockopt(fd,SOL_SOCKET,SO_SNDTIMEO,&timeout,socklen_t(MemoryLayout<timeval>.size))
    _=setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,socklen_t(MemoryLayout<timeval>.size))
    _=setsockopt(fd,SOL_SOCKET,SO_NOSIGPIPE,&one,socklen_t(MemoryLayout<Int32>.size))
    var address=sockaddr_un();address.sun_family=sa_family_t(AF_UNIX);address.sun_len=UInt8(MemoryLayout<sockaddr_un>.size)
    let pathBytes=Array(socketPath.utf8)+[0]
    withUnsafeMutableBytes(of:&address.sun_path){buffer in buffer.copyBytes(from:pathBytes)}
    let connected=withUnsafePointer(to:&address){$0.withMemoryRebound(to:sockaddr.self,capacity:1){Darwin.connect(fd,$0,socklen_t(MemoryLayout<sockaddr_un>.size))}}
    guard connected==0, var payload=try? JSONSerialization.data(withJSONObject:["method":"ingest_kakao","event":item]) else{return false}
    payload.append(10)
    var sent=0
    let wrote=payload.withUnsafeBytes{buffer -> Bool in
        while sent<payload.count{let n=Darwin.send(fd,buffer.baseAddress!.advanced(by:sent),payload.count-sent,0);if n<=0{return false};sent+=n};return true
    }
    guard wrote else{return false}
    var reply=Data(),buffer=[UInt8](repeating:0,count:1024)
    while reply.count<4096 {
        let n=Darwin.recv(fd,&buffer,buffer.count,0);if n<=0{return false}
        reply.append(contentsOf:buffer.prefix(n));if reply.contains(10){break}
    }
    guard let result=(try? JSONSerialization.jsonObject(with:reply)) as? [String:Any] else{return false}
    // A permanent rejection is handled; a storage/service failure must retry.
    return result["ok"] as? Bool == true || result["error"] as? String == "request_rejected"
}
@discardableResult func emit(_ item: [String: Any])->Bool {
    guard var bytes = try? JSONSerialization.data(withJSONObject: item, options: [.sortedKeys]) else { return false }
    bytes.append(10)
    if doctor { FileHandle.standardOutput.write(bytes) } else {
        if !sendToHub(item,hubSocket){return false}
        let statusItem: [String: Any] = (item["kind"] as? String) == "source_status" ? item : ["kind":"source_event_emitted"]
        if let status = try? JSONSerialization.data(withJSONObject: statusItem, options: [.sortedKeys]) {
            try? status.write(to: state.appendingPathComponent("receiver-status.json"), options: [.atomic])
            try? FileManager.default.setAttributes([.posixPermissions:0o600], ofItemAtPath:state.appendingPathComponent("receiver-status.json").path)
        }
    }
    return true
}
@Sendable func shape(_ value: Any, prefix: String = "", depth: Int = 0) -> [String: String] {
    if depth > 4 { return [:] }
    if let dict = value as? [String: Any] {
        return dict.reduce(into: [:]) { result, entry in
            let key = prefix.isEmpty ? entry.key : prefix + "." + entry.key
            result[key] = String(describing: type(of: entry.value))
            result.merge(shape(entry.value, prefix:key, depth:depth+1)) { a,_ in a }
        }
    }
    return [:]
}
var seen = Set<String>()
func poll() {
    var db: OpaquePointer?
    guard sqlite3_open_v2(database.path, &db, SQLITE_OPEN_READONLY, nil) == SQLITE_OK else {
        if db != nil { sqlite3_close(db) }
        emit(["kind":"source_status","status":"permission_or_storage_blocked","required_permission":"Full Disk Access for Kakao Mention Receiver","at":Date().timeIntervalSince1970]); return
    }
    defer { sqlite3_close(db) }
    sqlite3_busy_timeout(db, 2000)
    var statement: OpaquePointer?
    let query = "SELECT r.rec_id,r.data FROM record r JOIN app a ON r.app_id=a.app_id WHERE lower(a.identifier)='com.kakao.kakaotalkmac' AND r.data IS NOT NULL ORDER BY r.rec_id DESC LIMIT 256"
    guard sqlite3_prepare_v2(db, query, -1, &statement, nil) == SQLITE_OK else {
        emit(["kind":"source_status","status":"schema_unavailable","at":Date().timeIntervalSince1970]); return
    }
    defer { sqlite3_finalize(statement) }
    var rows: [(Int64, Data)] = []
    while sqlite3_step(statement) == SQLITE_ROW {
        let id=sqlite3_column_int64(statement,0), size=sqlite3_column_bytes(statement,1)
        if size > 0 && size < 1_000_000, let pointer=sqlite3_column_blob(statement,1) { rows.append((id,Data(bytes:pointer,count:Int(size)))) }
    }
    if doctor {
        let fields=rows.prefix(4).compactMap { _,data -> [String:String]? in
            guard let root=try? PropertyListSerialization.propertyList(from:data,options:[],format:nil) else {return nil}
            return shape(root)
        }
        emit(["kind":"source_status","status":"notification_store_readable","kakao_retained_count":rows.count,"field_shapes_only":fields]); return
    }
    var retained=Set<String>()
    for (_,data) in rows.reversed() {
        guard let root=(try? PropertyListSerialization.propertyList(from:data,options:[],format:nil)) as? [String:Any], let req=root["req"] as? [String:Any], let identity=req["iden"] as? String else {continue}
        let parts=identity.split(separator:"_")
        guard parts.count==2,Int64(parts[0]) != nil,Int64(parts[1]) != nil else {continue}
        retained.insert(identity)
        if seen.contains(identity) {continue}
        seen.insert(identity)
        let occurred: Double
        if let date=root["date"] as? Date {occurred=date.timeIntervalSince1970}
        else if let date=root["date"] as? Double,date>0 {occurred=date+978307200}
        else {continue}
        // Only numeric/bool/string metadata with mention-related keys is exported.
        // Unknown fields never become fabricated "actual mention" evidence.
        var mention: [String:Any] = [:]
        for (key,value) in req where key.lowercased().contains("mention") {
            if value is String || value is NSNumber {mention[key]=value}
        }
        if !emit(["source":"kakao_notification_store","room_id":String(parts[0]),"message_id":String(parts[1]),"body":req["body"] as? String ?? "","chat_name":req["titl"] as? String ?? "","occurred_at":occurred,"mention_metadata":mention,"sender_identity":"unverified"]){seen.remove(identity)}
    }
    if seen.count>4096 {seen.formIntersection(retained)}
    emit(["kind":"source_status","status":"watching_kakao_only","at":Date().timeIntervalSince1970])
}
if doctor {poll();exit(0)}
let app=NSApplication.shared
app.setActivationPolicy(.accessory)
poll()
let timer=Timer.scheduledTimer(withTimeInterval:3,repeats:true) { _ in
    poll()
}
RunLoop.main.add(timer,forMode:.common)
app.run()
