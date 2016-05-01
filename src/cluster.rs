
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::net::{SocketAddr,IpAddr,Ipv4Addr};
use std::error::Error;
use std::thread;
use def::mio::{EventLoop, Sender, Handler};
use def::*;
use def::eventual::Async;
use def::RCErrorType::*;
use def::TopologyChangeType::*;
use def::StatusChangeType::*;
use def::CqlResponseBody::*;
use def::CqlValue::*;
use node::{Node,ChannelPool};
use connection_pool::ConnectionPool;
use std::convert::AsRef;
use std::rc::Rc;
use std::boxed::Box;
use std::cell::RefCell;
use load_balancing::*;

type ArcMap = Arc<RwLock<BTreeMap<IpAddr,Node>>>;

pub struct Cluster{
	// Index of the current_node we are using
	current_node: IpAddr,	
	available_nodes: ArcMap,
	unavailable_nodes: ArcMap,
	channel_pool: Arc<ChannelPool>,
	// https://doc.rust-lang.org/error-index.html#E0038
	balancer:  Rc<LoadBalancing>
}

impl Cluster {

	pub fn new() -> Cluster{
		let availables 	 = Arc::new(RwLock::new(BTreeMap::new()));
		let unavailables = Arc::new(RwLock::new(BTreeMap::new()));

		//Start EventLoop<ConnectionPool>

        let mut event_loop_conn_pool : EventLoop<ConnectionPool> = 
        		EventLoop::new().ok().expect("Couldn't create event loop");
        let mut channel_pool = ChannelPool::new();

        channel_pool.add_channel(event_loop_conn_pool.channel());

        let arc_channel = Arc::new(channel_pool);

		//Start EventLoop<EventHandler>
        let mut event_loop : EventLoop<EventHandler> = 
        		EventLoop::new().ok().expect("Couldn't create event loop");
        let event_handler_channel = event_loop.channel();
        let mut event_handler = EventHandler::new(availables.clone(),unavailables.clone(),arc_channel.clone());

        // Only keep the event loop channel
        thread::spawn(move||{
                event_loop.run(&mut event_handler).ok().expect("Failed to start event loop");
            });

        


        // We will need the event loop to register a new socket
        // but on creating the thread we borrow the even_loop.
        // So we 'give away' the connection pool and keep the channel.
        let mut connection_pool = ConnectionPool::new(event_handler_channel);

        println!("Starting event loop...");
        // Only keep the event loop channel
        thread::spawn(move||{
                event_loop_conn_pool.run(&mut connection_pool).ok().expect("Failed to start event loop");
            });

		Cluster{
			available_nodes: availables.clone(),
			unavailable_nodes: unavailables.clone(),
			channel_pool: arc_channel,
			current_node: IpAddr::V4(Ipv4Addr::new(0,0,0,0)),
			balancer: Rc::new(RoundRobin{index:0})
		}
	}

	pub fn set_load_balancing(&mut self,balancer: Rc<LoadBalancing>){
		self.balancer = balancer;
	}

	pub fn are_available_nodes(&self) -> bool{
		self.available_nodes.read()
							.unwrap()
							.len() == 0
	}

	fn add_node(&self,ip: IpAddr) -> RCResult<CqlResponse>{
		let address = SocketAddr::new(ip,CQL_DEFAULT_PORT);
		let mut node = Node::new(address,self.channel_pool.clone());
		node.set_channel_pool(self.channel_pool.clone());

		//To-do: handle error
		let response = {
			try_unwrap!(node.connect().await())
		};

		match response {
			Ok(_) => {
				try_unwrap!(self.available_nodes.write())
							.insert(address.ip(),node);
			}
			Err(_) =>{
				try_unwrap!(self.unavailable_nodes.write())
							.insert(address.ip(),node);
			}
		}
		response
	}

	//This operation blocks
	pub fn connect_cluster(&mut self,address: SocketAddr) -> RCResult<CqlResponse>{
		if self.are_available_nodes(){
			self.current_node = address.ip();
			let connect_response = self.add_node(self.current_node);
			match connect_response{
				Ok(_) => {
					let peers = try_unwrap!(try_unwrap!(self.get_peers().await()));
					//println!("Peers: {:?}",peers);
					//TODO: handle errors with a macro
					let ip_nodes = try_unwrap!(self.parse_nodes(peers));
					self.create_nodes(ip_nodes);
				},
				Err(_) =>{
					()
				}
			}
			return connect_response;
		}
		else{
			return Err(RCError::new("Already connected to cluster", ClusterError)) 
		}
	}

	fn parse_nodes(&self,response: CqlResponse) -> RCResult<Vec<IpAddr>>{
		let mut nodes = Vec::new();
		match response.body {
			ResultRows(cql_rows) => {
				if cql_rows.rows.len() > 0 {
					let rows = cql_rows.rows.clone();
					for row in rows {
						println!("Col: {:?}",row);
						match *row.cols.get(0).unwrap() {
							CqlInet(Some(ip)) => {
								nodes.push(ip);
							},
							_ => return Err(RCError::new("Error CqlResponse contains no rows", ReadError)),
						}
					}
					Ok(nodes)
				}
				else{
					Err(RCError::new("Error CqlResponse contains no rows", ReadError))
				}
			},
			_ => Err(RCError::new("Error CqlResponse type must be ResultRows", ClusterError)),
		}
	}

	fn create_nodes(&mut self,ips: Vec<IpAddr>){
		for ip in ips {
		    self.add_node(ip);
		}
	}

	fn update_current_node(&mut self){
		self.current_node = Rc::get_mut(&mut self.balancer).unwrap()
							.select_node(&self.available_nodes.read().unwrap());
	}

	pub fn start_cluster(&mut self){
		//self.run_event_loop();
	}

	pub fn get_peers(&mut self) -> CassFuture{
		let map = self.available_nodes
			   .read()
			   .unwrap();
		let node = map.get(&self.current_node)
					   .unwrap();
		node.get_peers()
	}


	pub fn exec_query(&mut self, query_str: &str, con: Consistency) -> CassFuture {
		let map = self.available_nodes
					   .read()
					   .unwrap();
		let node = map.get(&self.current_node)
					   .unwrap();
					   
		node.exec_query(query_str,con)
	}

	pub fn register(&mut self) -> CassFuture{
		let map = self.available_nodes
			   		.read()
			   		.unwrap();
		let node = 	map.get(&self.current_node)
			   			.unwrap();
		node.send_register(Vec::new())
	}

	// This temporal until I return some type
	pub fn show_cluster_information(&self){
		let map_availables = 
			self.available_nodes
	   			.read()
	   			.unwrap();
	   	let map_unavailables = 
	   		self.unavailable_nodes
	   			.read()
	   			.unwrap();
		println!("--------------Available nodes-----------");
		println!("Address");
		for node in map_availables.iter() {
			print!("{:?}\t",node.1.get_sock_addr());
		}
		println!("");
		println!("----------------------------------------");

		println!("--------------Unavailable nodes----------");
		println!("Address");
		for node in map_unavailables.iter() {
			print!("{:?}\t",node.1.get_sock_addr());
		}
		println!("----------------------------------------");
	}
}

struct EventHandler{
	available_nodes: ArcMap,
	unavailable_nodes: ArcMap,
	channel_pool: Arc<ChannelPool>
}

impl EventHandler{
	fn new(availables: ArcMap,unavailables: ArcMap,channel_pool : Arc<ChannelPool>) -> EventHandler{
		EventHandler{
			available_nodes: availables,
			unavailable_nodes: unavailables,
			channel_pool: channel_pool
		}
	}
}

impl Handler for EventHandler {
    type Timeout = ();

    type Message = CqlEvent; 

    fn notify(&mut self, event_loop: &mut EventLoop<EventHandler>, msg: CqlEvent) {
    	match msg {
    		CqlEvent::TopologyChange(change_type,socket_addr) =>{
    			match change_type{
    				NewNode =>{
    					let mut map = self.available_nodes
					   		.write()
					   		.unwrap();
    					map.insert(socket_addr.ip(),
    							Node::new(socket_addr,self.channel_pool.clone()));
    				},
    				RemovedNode =>{
    					let mut map = self.available_nodes
					   		.write()
					   		.unwrap();
    					map.remove(&socket_addr.ip());
    				},
    				MovedNode =>{
    					//Not sure about this.
    					let mut map = self.available_nodes
					   		.write()
					   		.unwrap();
    					map.insert(socket_addr.ip(),
    							Node::new(socket_addr,self.channel_pool.clone()));
    				},
    				Unknown => ()
    			}
			},
			CqlEvent::StatusChange(change_type,socket_addr) =>{
				//Need for a unavailable_nodes list (down)
				match change_type{
					Up =>{
						let mut map_unavailable = self.available_nodes
					   		.write()
					   		.unwrap();
					   	//To-do: treat error if node doesn't exist
    					let node = map_unavailable.remove(&socket_addr.ip()).unwrap();

    					let mut map_available = 
	    					self.available_nodes
								.write()
								.unwrap();
    					map_available.insert(node.get_sock_addr().ip(),node);
					},
					Down =>{
						let mut map_available = self.available_nodes
					   		.write()
					   		.unwrap();
					   	//To-do: treat error if node doesn't exist
    					let node = map_available.remove(&socket_addr.ip()).unwrap();

    					let mut map_unavailable = 
	    					self.unavailable_nodes
								.write()
								.unwrap();
    					map_unavailable.insert(node.get_sock_addr().ip(),node);
					},
					UnknownStatus => ()
				}
			},
			CqlEvent::SchemaChange(change_type,socket_addr) =>{
				println!("Schema changes are not handled yet.");
			},
			CqlEvent::UnknownEvent=> {
				println!("We've got an UnkownEvent");
			}
		}
   }
}