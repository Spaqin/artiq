mod rtio_mgt;
mod rtio_dma;
mod session;
#[cfg(any(has_rtio_moninj, has_drtio))]
mod moninj;
#[cfg(has_rtio_analyzer)]
mod analyzer;

pub fn start(io: Io) {
    #[cfg(has_drtio)]
    let drtio_routing_table = urc::Urc::new(RefCell::new(
        drtio_routing::config_routing_table(csr::DRTIO.len())));
    #[cfg(not(has_drtio))]
    let drtio_routing_table = urc::Urc::new(RefCell::new(
        drtio_routing::RoutingTable::default_empty()));
    let up_destinations = urc::Urc::new(RefCell::new(
        [false; drtio_routing::DEST_COUNT]));
    #[cfg(has_drtio_routing)]
    drtio_routing::interconnect_disable_all();
    let aux_mutex = sched::Mutex::new();

    let ddma_mutex = sched::Mutex::new();

    rtio_mgt::startup(&io, &aux_mutex, &drtio_routing_table, &up_destinations, &ddma_mutex);
    {
        let aux_mutex = aux_mutex.clone();
        let drtio_routing_table = drtio_routing_table.clone();
        let up_destinations = up_destinations.clone();
        let ddma_mutex = ddma_mutex.clone();
        io.spawn(16384, move |io| { session::thread(io, &aux_mutex, &drtio_routing_table, &up_destinations, &ddma_mutex) });
    }
    #[cfg(any(has_rtio_moninj, has_drtio))]
    {
        let aux_mutex = aux_mutex.clone();
        let ddma_mutex = ddma_mutex.clone();
        let drtio_routing_table = drtio_routing_table.clone();
        io.spawn(4096, move |io| { moninj::thread(io, &aux_mutex, &ddma_mutex, &drtio_routing_table) });
    }
    #[cfg(has_rtio_analyzer)]
    {
        let aux_mutex = aux_mutex.clone();
        let drtio_routing_table = drtio_routing_table.clone();
        let up_destinations = up_destinations.clone();
        let ddma_mutex = ddma_mutex.clone();
        io.spawn(8192, move |io| { analyzer::thread(io, &aux_mutex, &ddma_mutex, &drtio_routing_table, &up_destinations) });
    }

    #[cfg(has_grabber)]
    io.spawn(4096, grabber_thread);
}