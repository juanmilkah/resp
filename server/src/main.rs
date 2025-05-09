use std::{
    collections::HashMap,
    env::args,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    sync::Arc,
};

use server_lib::{
    DataType, Request, Storage, StorageEntry, StorageOptions, ThreadPool, parse_request,
};

fn null_array() -> Vec<u8> {
    b"*-1\r\n".to_vec()
}

// -Error message\r\n
fn simple_error(s: &str) -> Vec<u8> {
    let mut err = String::new();
    err.push('-');
    err.push_str(s);
    err.push_str("\r\n");

    err.as_bytes().to_vec()
}

fn bstring(s: Option<String>) -> Vec<u8> {
    match s {
        Some(rsp) => {
            let mut s = String::new();
            s.push('$');
            let len = rsp.len();
            s.push_str(&len.to_string());
            s.push_str("\r\n");
            s.push_str(&rsp);
            s.push_str("\r\n");

            s.as_bytes().to_vec()
        }
        None => {
            // null $-1\r\n
            b"$-1\r\n".to_vec()
        }
    }
}

// *<number-of-elements>\r\n<element-1>...<element-n>
struct Array(String);

impl Array {
    fn new(elems: &[String]) -> Self {
        let mut arr = String::new();
        let terminator = "\r\n";

        arr.push('*');
        arr.push_str(&elems.len().to_string());
        arr.push_str(terminator);

        for s in elems {
            arr.push_str(s);
        }

        Array(arr)
    }
}

fn get_data(req: &Request) -> Option<String> {
    match req.data_type {
        DataType::BulkStrings => req
            .content
            .as_ref()
            .map(|content| String::from_utf8_lossy(content).to_string()),
        _ => None,
    }
}

enum Command {
    Get,
    Set,
    Delete,
    SetList,
    GetList,
    DeleteList,
}

fn process_request(req: &[Request], storage: Arc<parking_lot::RwLock<Storage>>) -> Vec<u8> {
    if req.is_empty() {
        // return null array *-1\r\n
        return null_array();
    }

    let mut i = 0;
    match req[i].data_type {
        DataType::BulkStrings => {}
        _ => return simple_error("Invalid command"),
    };

    let cmd = match get_data(&req[i]) {
        Some(data) => match data.as_str() {
            "GET" => Command::Get,
            "SET" => Command::Set,
            "DELETE" => Command::Delete,
            "GETLIST" => Command::GetList,
            "SETLIST" => Command::SetList,
            "DELETELIST" => Command::DeleteList,
            _ => return simple_error("unknow command: {data}"),
        },
        None => return bstring(None),
    };

    i += 1;
    match cmd {
        Command::Get => {
            if req.len() < 2 {
                return simple_error("Missing key");
            }
            if let Some(key) = get_data(&req[i]) {
                let key = storage.write().get_key(&key);
                if let Some(key) = key {
                    return bstring(Some(key.value));
                }
                bstring(None)
            } else {
                bstring(None)
            }
        }
        Command::Set => {
            if req.len() < 3 {
                return simple_error("Missing values");
            }

            match get_data(&req[i]) {
                Some(key) => match get_data(&req[i + 1]) {
                    Some(value) => {
                        storage.write().insert_entry(key, value);
                        bstring(Some("SUCCESS".to_string()))
                    }
                    None => simple_error("Missing value"),
                },
                None => simple_error("Missing key"),
            }
        }
        Command::Delete => {
            if req.len() < 2 {
                return simple_error("Invalid values");
            }

            match get_data(&req[i]) {
                Some(key) => {
                    storage.write().remove_entry(&key);
                    bstring(Some("SUCCESS".to_string()))
                }
                None => simple_error("Missing key"),
            }
        }
        Command::SetList => {
            let mut map = HashMap::new();
            while i < req.len() {
                if let Some(key) = get_data(&req[i]) {
                    i += 1;
                    if let Some(value) = get_data(&req[i]) {
                        i += 1;
                        map.insert(key, value);
                    }
                } else {
                    i += 2;
                }
            }

            storage.write().insert_entries(map);

            bstring(Some("SUCCESS".to_string()))
        }
        Command::GetList => {
            let mut keys = Vec::new();
            while i < req.len() {
                if let Some(key) = get_data(&req[i]) {
                    keys.push(key);
                }
                i += 1;
            }

            let entries: Vec<(String, Option<StorageEntry>)> = storage.write().get_entries(&keys);

            let mut result = Vec::new();
            for (_, entry) in entries {
                if let Some(e) = entry {
                    let v = String::from_utf8_lossy(&bstring(Some(e.value))).to_string();
                    result.push(v);
                } else {
                    let v = String::from_utf8_lossy(&bstring(None)).to_string();
                    result.push(v);
                }
            }

            let arr = Array::new(&result);
            arr.0.as_bytes().to_vec()
        }
        Command::DeleteList => {
            let mut keys = Vec::new();
            while i < req.len() {
                if let Some(key) = get_data(&req[i]) {
                    keys.push(key);
                }
                i += 1;
            }

            storage.write().remove_entries(&keys);

            bstring(Some("SUCCESS".to_string()))
        }
    }
}

fn handle_client(mut stream: TcpStream, storage: Arc<parking_lot::RwLock<Storage>>) {
    // 1MB
    let mut buffer = [0; 1024 * 1024];

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => match parse_request(&buffer[..n]) {
                Ok(req) => {
                    let storage = Arc::clone(&storage);
                    let response = process_request(&req, storage);

                    if let Err(e) = stream.write_all(&response) {
                        eprintln!("ERROR: {e}");
                    }
                }
                Err(err) => {
                    let err = format!("Error parsing request: {err}");
                    if let Err(e) = stream.write_all(err.as_bytes()) {
                        eprintln!("ERROR: {e}");
                    }
                }
            },
            Err(e) => eprintln!("ERROR: {e}"),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = args().collect();
    let mut thread_count = 4;

    for i in 1..args.len() {
        if args[i] == "--threads" && i + 1 < args.len() {
            thread_count = args[i + 1].parse().unwrap_or(4);
        }
    }

    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 7878));
    let listener = TcpListener::bind(addr)?;
    println!("Server listening on {addr}");

    let options = StorageOptions::default();
    let storage = Arc::new(parking_lot::RwLock::new(Storage::new(options)));
    let pool = ThreadPool::new(thread_count); // 4 threads

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let storage = Arc::clone(&storage);
                pool.execute(|| handle_client(stream, storage));
            }
            Err(e) => eprintln!("ERROR: {e}"),
        }
    }

    Ok(())
}
