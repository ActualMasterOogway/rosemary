use std::ffi::c_void;
use std::sync::Mutex;

use coreaudio::ComponentDescription;

/// `OSStatus` values from `<AudioToolbox/AUGraph.h>`, and `noErr`.
pub const SUCCESS: i32 = 0;
const PARAM_ERR: i32 = -50;
const NODE_NOT_FOUND: i32 = -10860;
const INVALID_CONNECTION: i32 = -10861;
const CANNOT_DO_IN_CURRENT_CONTEXT: i32 = -10863;
const INVALID_AUDIO_UNIT: i32 = -10864;

/// `AUNode`, which is a `SInt32` a graph hands out and the caller passes back.
type Node = i32;

struct NodeEntry {
    node: Node,
    description: ComponentDescription,
    /// The `AudioUnit` behind it, or null where this runtime has no such
    /// component. See [`AUGraphOpen`].
    unit: *mut c_void,
}

/// One connection: an output bus of one node feeding an input bus of another.
struct Connection {
    source: Node,
    source_bus: u32,
    destination: Node,
    destination_bus: u32,
}

#[derive(Default)]
struct Graph {
    nodes: Vec<NodeEntry>,
    connections: Vec<Connection>,
    /// The number the next `AUGraphAddNode` hands out. One-based, because zero
    /// is the value a caller's uninitialised `AUNode` holds.
    next_node: Node,
    open: bool,
    initialized: bool,
    running: bool,
}

// SAFETY: `unit` is a handle `coreaudio` made and is only ever passed back to
// it; everything else is plain data. All of it is behind the registry's mutex.
unsafe impl Send for Graph {}

/// Every graph handed out, by the handle the caller holds - the address of its
/// own box, which is stable for as long as the graph lives.
static GRAPHS: Mutex<Vec<(usize, Box<Graph>)>> = Mutex::new(Vec::new());

/// Run `f` over the graph `handle` names.
fn with_graph<T>(handle: *mut c_void, f: impl FnOnce(&mut Graph) -> T) -> Option<T> {
    let mut graphs = GRAPHS.lock().unwrap_or_else(|e| e.into_inner());
    let graph = graphs.iter_mut().find(|(h, _)| *h == handle as usize)?;
    Some(f(&mut graph.1))
}

impl Graph {
    fn node(&self, node: Node) -> Option<&NodeEntry> {
        self.nodes.iter().find(|entry| entry.node == node)
    }

    /// Make the unit for every node that has none yet.
    ///
    /// Answers the description of the first node it could not build, which is
    /// what the caller is told about.
    fn instantiate(&mut self) -> Option<ComponentDescription> {
        let mut unbuilt = None;
        for entry in &mut self.nodes {
            if !entry.unit.is_null() {
                continue;
            }
            // SAFETY: a readable description, and a writable handle.
            let component = unsafe {
                coreaudio::AudioComponentFindNext(std::ptr::null_mut(), &entry.description)
            };
            if component.is_null() {
                unbuilt.get_or_insert(entry.description);
                continue;
            }
            let mut unit = std::ptr::null_mut();
            // SAFETY: a component that was just found, and a writable handle.
            let status = unsafe { coreaudio::AudioComponentInstanceNew(component, &mut unit) };
            match status == SUCCESS && !unit.is_null() {
                true => entry.unit = unit,
                false => {
                    unbuilt.get_or_insert(entry.description);
                }
            }
        }
        unbuilt
    }
}

/// Say which component a graph asked for and this runtime has not got.
///
/// Once per description: a graph is opened and re-opened, and a program that
/// adds a voice per sound would otherwise print this per sound.
fn report_unbuilt(description: &ComponentDescription) {
    static SAID: Mutex<Vec<ComponentDescription>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(description) {
        return;
    }
    said.push(*description);
    eprintln!(
        "rosetta3: audio: this program's audio graph wants an audio unit of type {} subtype {} \
         ({}), which this runtime does not implement - the graph runs without it",
        coreaudio::trace_code(description.kind),
        coreaudio::trace_code(description.subtype),
        coreaudio::trace_code(description.manufacturer),
    );
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn NewAUGraph(out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return PARAM_ERR;
    }
    let mut graph = Box::new(Graph {
        next_node: 1,
        ..Graph::default()
    });
    let handle = graph.as_mut() as *mut Graph as *mut c_void;
    GRAPHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, graph));
    // SAFETY: a writable handle, as just checked.
    unsafe { *out = handle };
    coreaudio::trace(format_args!("NewAUGraph -> {handle:p}"), SUCCESS);
    SUCCESS
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn DisposeAUGraph(handle: *mut c_void) -> i32 {
    let mut graphs = GRAPHS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(index) = graphs.iter().position(|(h, _)| *h == handle as usize) else {
        return INVALID_AUDIO_UNIT;
    };
    let (_, graph) = graphs.remove(index);
    for entry in &graph.nodes {
        if !entry.unit.is_null() {
            // SAFETY: a unit this module made through `coreaudio`.
            unsafe { coreaudio::AudioComponentInstanceDispose(entry.unit) };
        }
    }
    coreaudio::trace(format_args!("DisposeAUGraph({handle:p})"), SUCCESS);
    SUCCESS
}

/// `OSStatus AUGraphAddNode(AUGraph, const AudioComponentDescription *, AUNode *)`
///
/// The unit is not made here: `AUGraphOpen` is what opens the components, and a
/// node added to an already-open graph is opened on the spot to match.
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphAddNode(
    handle: *mut c_void,
    description: *const ComponentDescription,
    out: *mut Node,
) -> i32 {
    if description.is_null() || out.is_null() {
        return PARAM_ERR;
    }
    // SAFETY: a readable description, as just checked.
    let description = unsafe { *description };
    let Some(node) = with_graph(handle, |graph| {
        let node = graph.next_node;
        graph.next_node += 1;
        graph.nodes.push(NodeEntry {
            node,
            description,
            unit: std::ptr::null_mut(),
        });
        if graph.open
            && let Some(unbuilt) = graph.instantiate()
        {
            report_unbuilt(&unbuilt);
        }
        node
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    // SAFETY: a writable node, as just checked.
    unsafe { *out = node };
    coreaudio::trace(
        format_args!(
            "AUGraphAddNode({handle:p}, type {} subtype {}) -> node {node}",
            coreaudio::trace_code(description.kind),
            coreaudio::trace_code(description.subtype),
        ),
        SUCCESS,
    );
    SUCCESS
}

/// `OSStatus AUGraphOpen(AUGraph)` - instantiate every node's component.
///
/// A node whose component this runtime has not got is left with a null unit and
/// named once on the error stream. The graph still opens: the rest of it works,
/// and a program told its whole graph failed turns its audio off entirely,
/// which is a worse outcome than one missing unit.
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphOpen(handle: *mut c_void) -> i32 {
    let Some(unbuilt) = with_graph(handle, |graph| {
        graph.open = true;
        graph.instantiate()
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    if let Some(description) = unbuilt {
        report_unbuilt(&description);
    }
    coreaudio::trace(format_args!("AUGraphOpen({handle:p})"), SUCCESS);
    SUCCESS
}

/// `OSStatus AUGraphNodeInfo(AUGraph, AUNode, AudioComponentDescription *,
/// AudioUnit *)` - either output may be null, and a caller usually wants only
/// the unit.
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphNodeInfo(
    handle: *mut c_void,
    node: Node,
    description: *mut ComponentDescription,
    unit: *mut *mut c_void,
) -> i32 {
    let Some(found) = with_graph(handle, |graph| {
        graph
            .node(node)
            .map(|entry| (entry.description, entry.unit))
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    let Some((found_description, found_unit)) = found else {
        return NODE_NOT_FOUND;
    };
    if !description.is_null() {
        // SAFETY: a writable description, as just checked.
        unsafe { *description = found_description };
    }
    if !unit.is_null() {
        // SAFETY: a writable handle, as just checked.
        unsafe { *unit = found_unit };
    }
    SUCCESS
}

/// `OSStatus AUGraphConnectNodeInput(AUGraph, AUNode, UInt32, AUNode, UInt32)`
///
/// The connection is what makes the graph play: the device pulls the output
/// unit, which pulls whatever is connected to its input, and so on back through
/// the mixer to each voice. A connection replaces whatever fed that destination
/// bus, as CoreAudio's does.
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphConnectNodeInput(
    handle: *mut c_void,
    source: Node,
    source_bus: u32,
    destination: Node,
    destination_bus: u32,
) -> i32 {
    let Some(status) = with_graph(handle, |graph| {
        if graph.node(source).is_none() || graph.node(destination).is_none() {
            return NODE_NOT_FOUND;
        }
        let (Some(from), Some(to)) = (
            graph.node(source).map(|entry| entry.unit),
            graph.node(destination).map(|entry| entry.unit),
        ) else {
            return NODE_NOT_FOUND;
        };
        // A node whose component this runtime could not build has no unit, and
        // a connection to or from it is one that cannot carry audio.
        if from.is_null() || to.is_null() {
            return INVALID_CONNECTION;
        }
        if !coreaudio::connect(from, to, destination_bus) {
            return INVALID_CONNECTION;
        }
        // One output bus feeds one input bus. A source left connected to a
        // second destination would be pulled twice per buffer, and each pull
        // takes fresh frames from the voice behind it - so both destinations
        // would play every other chunk of it.
        for stale in graph.connections.iter().filter(|c| {
            c.source == source && c.source_bus == source_bus && c.destination != destination
        }) {
            if let Some(unit) = graph.node(stale.destination).map(|entry| entry.unit)
                && !unit.is_null()
            {
                coreaudio::disconnect(unit, stale.destination_bus);
            }
        }
        graph.connections.retain(|c| {
            !(c.destination == destination && c.destination_bus == destination_bus)
                && !(c.source == source && c.source_bus == source_bus)
        });
        graph.connections.push(Connection {
            source,
            source_bus,
            destination,
            destination_bus,
        });
        SUCCESS
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    coreaudio::trace(
        format_args!(
            "AUGraphConnectNodeInput({handle:p}, {source}:{source_bus} -> \
             {destination}:{destination_bus})"
        ),
        status,
    );
    status
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphDisconnectNodeInput(
    handle: *mut c_void,
    destination: Node,
    destination_bus: u32,
) -> i32 {
    let Some(status) = with_graph(handle, |graph| {
        let before = graph.connections.len();
        graph
            .connections
            .retain(|c| !(c.destination == destination && c.destination_bus == destination_bus));
        if graph.connections.len() == before {
            return INVALID_CONNECTION;
        }
        if let Some(to) = graph.node(destination).map(|entry| entry.unit)
            && !to.is_null()
        {
            coreaudio::disconnect(to, destination_bus);
        }
        SUCCESS
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    status
}

/// `OSStatus AUGraphInitialize(AUGraph)` - initialize each unit in turn.
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphInitialize(handle: *mut c_void) -> i32 {
    let Some(status) = with_graph(handle, |graph| {
        if !graph.open {
            return CANNOT_DO_IN_CURRENT_CONTEXT;
        }
        let mut status = SUCCESS;
        for entry in &graph.nodes {
            if entry.unit.is_null() {
                continue;
            }
            // SAFETY: a unit this module made through `coreaudio`.
            let one = unsafe { coreaudio::AudioUnitInitialize(entry.unit) };
            if one != SUCCESS && status == SUCCESS {
                status = one;
            }
        }
        graph.initialized = status == SUCCESS;
        status
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    coreaudio::trace(format_args!("AUGraphInitialize({handle:p})"), status);
    status
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphUninitialize(handle: *mut c_void) -> i32 {
    let Some(()) = with_graph(handle, |graph| {
        for entry in &graph.nodes {
            if !entry.unit.is_null() {
                // SAFETY: a unit this module made through `coreaudio`.
                unsafe { coreaudio::AudioUnitUninitialize(entry.unit) };
            }
        }
        graph.initialized = false;
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    SUCCESS
}

/// `OSStatus AUGraphStart(AUGraph)` - start the output units, which is what
/// pulls the rest of the graph.
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphStart(handle: *mut c_void) -> i32 {
    let Some(status) = with_graph(handle, |graph| {
        if !graph.initialized {
            return CANNOT_DO_IN_CURRENT_CONTEXT;
        }
        let mut status = SUCCESS;
        for entry in &graph.nodes {
            if entry.unit.is_null() {
                continue;
            }
            // SAFETY: a unit this module made through `coreaudio`.
            let one = unsafe { coreaudio::AudioOutputUnitStart(entry.unit) };
            if one != SUCCESS && status == SUCCESS {
                status = one;
            }
        }
        graph.running = status == SUCCESS;
        status
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    coreaudio::trace(format_args!("AUGraphStart({handle:p})"), status);
    status
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphStop(handle: *mut c_void) -> i32 {
    let Some(()) = with_graph(handle, |graph| {
        for entry in &graph.nodes {
            if !entry.unit.is_null() {
                // SAFETY: a unit this module made through `coreaudio`.
                unsafe { coreaudio::AudioOutputUnitStop(entry.unit) };
            }
        }
        graph.running = false;
    }) else {
        return INVALID_AUDIO_UNIT;
    };
    coreaudio::trace(format_args!("AUGraphStop({handle:p})"), SUCCESS);
    SUCCESS
}

/// `OSStatus AUGraphUpdate(AUGraph, Boolean *)` - apply pending changes.
///
/// Everything here takes effect when it is asked for, so there is never
/// anything pending and the answer is always "done".
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphUpdate(handle: *mut c_void, updated: *mut u8) -> i32 {
    if with_graph(handle, |_| ()).is_none() {
        return INVALID_AUDIO_UNIT;
    }
    if !updated.is_null() {
        // SAFETY: a writable byte, as just checked.
        unsafe { *updated = 1 };
    }
    SUCCESS
}

/// `OSStatus AUGraphIsRunning(AUGraph, Boolean *)` and its two siblings.
#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphIsRunning(handle: *mut c_void, running: *mut u8) -> i32 {
    let Some(is) = with_graph(handle, |graph| graph.running) else {
        return INVALID_AUDIO_UNIT;
    };
    if !running.is_null() {
        // SAFETY: a writable byte, as just checked.
        unsafe { *running = is as u8 };
    }
    SUCCESS
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphIsOpen(handle: *mut c_void, open: *mut u8) -> i32 {
    let Some(is) = with_graph(handle, |graph| graph.open) else {
        return INVALID_AUDIO_UNIT;
    };
    if !open.is_null() {
        // SAFETY: a writable byte, as just checked.
        unsafe { *open = is as u8 };
    }
    SUCCESS
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn AUGraphIsInitialized(handle: *mut c_void, initialized: *mut u8) -> i32 {
    let Some(is) = with_graph(handle, |graph| graph.initialized) else {
        return INVALID_AUDIO_UNIT;
    };
    if !initialized.is_null() {
        // SAFETY: a writable byte, as just checked.
        unsafe { *initialized = is as u8 };
    }
    SUCCESS
}

exports::exports! {
    functions {
        "NewAUGraph" => NewAUGraph,
        "DisposeAUGraph" => DisposeAUGraph,
        "AUGraphOpen" => AUGraphOpen,
        "AUGraphAddNode" => AUGraphAddNode,
        "AUGraphNodeInfo" => AUGraphNodeInfo,
        "AUGraphConnectNodeInput" => AUGraphConnectNodeInput,
        "AUGraphDisconnectNodeInput" => AUGraphDisconnectNodeInput,
        "AUGraphInitialize" => AUGraphInitialize,
        "AUGraphUninitialize" => AUGraphUninitialize,
        "AUGraphStart" => AUGraphStart,
        "AUGraphStop" => AUGraphStop,
        "AUGraphUpdate" => AUGraphUpdate,
        "AUGraphIsRunning" => AUGraphIsRunning,
        "AUGraphIsOpen" => AUGraphIsOpen,
        "AUGraphIsInitialized" => AUGraphIsInitialized,
    }
}
