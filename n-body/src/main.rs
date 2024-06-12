mod tree;

use clap::Parser;
use mpi::{datatype::PartitionMut, topology::SimpleCommunicator, traits::*};
use rand::{thread_rng, Rng};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    iter::repeat,
    time::{Duration, Instant},
};
use tree::TreeNode;

const G: f64 = 6.67e-11f64;
const ROOT_RANK: usize = 0;

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
struct Args {
    #[arg(short = 'M', default_value_t = 1e3f64)]
    mass_max: f64,

    #[arg(short = 'P', default_value_t = 1e2f64)]
    pos_max: f64,

    #[arg(short = 'S', default_value_t = 1e0f64)]
    velocity_max: f64,

    #[arg(short = 'n', default_value_t = 1000)]
    n_bodies: usize,

    #[arg(short = 's', default_value_t = 1000)]
    n_steps: usize,

    #[arg(short = 'l', default_value_t = 0.1)]
    step_time: f64,

    #[arg(short = 'p', action)]
    print: bool,

    #[arg(short = 'T', default_value_t = 0.5)]
    theta: f64,

    #[arg(short = 't')]
    threads_per_node: Option<usize>,
}

#[derive(Clone, Debug, Default, Equivalence, Deserialize, Serialize)]
struct Body {
    id: usize,
    mass: f64,
    position: [f64; 2],
    velocity: [f64; 2],
}

/// Generates a float vector of the given length within a given min-max range.
///
/// * `n`: Length of the output vector.
/// * `min`: Minimum of the generated values.
/// * `max`: Maximum of the generated values.
fn generate_random_bounded(n: usize, min: f64, max: f64) -> Vec<f64> {
    let mut result = vec![0f64; n];
    thread_rng().fill(&mut result[..]);

    result.iter().map(|x| x * (max - min) + min).collect()
}

/// Gather outer bounds of all given bodies
///
/// * `positions`: Positions of all bodies.
fn get_bounds(positions: &[[f64; 2]]) -> [[f64; 2]; 2] {
    [
        [
            positions
                .iter()
                .map(|p| p[0])
                .min_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap(),
            positions
                .iter()
                .map(|p| p[0])
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap(),
        ],
        [
            positions
                .iter()
                .map(|p| p[1])
                .min_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap(),
            positions
                .iter()
                .map(|p| p[1])
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap(),
        ],
    ]
}

static mut SUBTREE_DURATIONS: Vec<Duration> = vec![];
static mut MERGE_DURATIONS: Vec<Duration> = vec![];
static mut CALC_DURATIONS: Vec<Duration> = vec![];
static mut GATHER_DURATIONS: Vec<Duration> = vec![];

/// Execute one parallelized step of the Barnes-Hut algorithm.
///
/// 1. Create a tree from the local bodies.
/// 2. Serialize the tree.
/// 3. Share tree with other processes and gather from them.
/// 4. Deserialize others' trees.
/// 5. Merge others' trees into own.
/// 6. Calculate forces recursively for the local bodies.
///
/// * `timestep`: Size of timesteps
/// * `theta`: Theta threshold of the algorithm
/// * `local_bodies`: Bodies to compute values for.
/// * `root`: Root tree node which already contains size and center respecting ALL bodies.
/// * `n_threads`: Number of parallel threads available.
/// * `world`: MPI communicator.
fn barnes_hut(
    timestep: f64,
    theta: f64,
    local_bodies: &mut Vec<Body>,
    root: &mut TreeNode,
    n_threads: usize,
    world: &SimpleCommunicator,
) {
    let mut start_time = std::time::Instant::now();

    let bodies_per_thread = local_bodies.len() / n_threads + 1;

    // create NUM_THREADS trees in parallel
    let thread_trees = local_bodies
        .par_chunks(bodies_per_thread)
        .map(|bs| {
            let mut thread_root = root.clone();

            bs.iter().for_each(|b| {
                if b.mass > 0f64 {
                    thread_root.insert(b);
                }
            });

            thread_root
        })
        .collect::<Vec<TreeNode>>();

    // merge the trees to a big tree
    for tree in thread_trees {
        root.merge(tree);
    }

    unsafe {
        SUBTREE_DURATIONS.push(start_time.elapsed());
    }

    start_time = Instant::now();

    if world.size() > 1 {
        // serialize own tree
        let serialized = bitcode::serialize(&root).unwrap();

        // send length of serialization to all processes
        let mut serialized_lengths = vec![0i32; world.size() as usize];
        world.all_gather_into(&(serialized.len() as i32), &mut serialized_lengths);

        // root gathers all serialized trees
        let total_serialized_length = serialized_lengths.iter().sum::<i32>() as usize;
        let mut all_trees_buf = vec![0u8; total_serialized_length];
        let offsets: Vec<i32> = serialized_lengths
            .iter()
            .scan(0, |acc, &x| {
                let tmp = *acc;
                *acc += x;
                Some(tmp)
            })
            .collect();
        let mut partition =
            PartitionMut::new(&mut all_trees_buf[..], serialized_lengths, &offsets[..]);
        world.all_gather_varcount_into(&serialized, &mut partition);

        // each process deserializes all trees
        let world_size = world.size();
        let all_trees = offsets
            .par_iter()
            .enumerate()
            .map(|(i, offset)| {
                let end_offset = if i == world_size as usize - 1 {
                    total_serialized_length
                } else {
                    offsets[i + 1] as usize
                };
                bitcode::deserialize::<TreeNode>(&all_trees_buf[*offset as usize..end_offset])
                    .unwrap()
            })
            .collect::<Vec<TreeNode>>();

        // merge all parsed trees into the local root tree, consuming the parsed trees
        for (i, tree) in all_trees.into_iter().enumerate() {
            if i != world.rank() as usize {
                root.merge(tree);
            }
        }
    }

    unsafe {
        MERGE_DURATIONS.push(start_time.elapsed());
    }

    start_time = Instant::now();

    // calculate forces, velocity and positions
    local_bodies.par_iter_mut().for_each(|b| {
        if b.mass == 0f64 {
            return;
        }

        let f = root.calculate_force(b, theta);
        b.velocity = calc_velocity(&b.velocity, &f, b.mass, timestep);
        b.position = calc_position(&b.velocity, &b.position, timestep);
    });

    unsafe {
        CALC_DURATIONS.push(start_time.elapsed());
    }
}

fn main() {
    let universe = mpi::initialize().unwrap();
    let world = universe.world();
    let n_nodes = world.size();
    let rank = world.rank() as usize;

    let is_root = rank == ROOT_RANK;

    // parse hyperparameteres; shared between all processes without sending them actively
    let args = Args::parse();

    if let Some(threads) = args.threads_per_node {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .unwrap();
    }

    let n_threads = args.threads_per_node.unwrap_or(1);

    let start_time = Instant::now();

    // we add zero weight bodies at the end
    // so that all processes get the same amount of bodies
    let bodies_per_proc = (args.n_bodies as f64 / n_nodes as f64).ceil() as usize;
    let filled_n = bodies_per_proc * n_nodes as usize;
    let extra_n = filled_n - args.n_bodies;

    let mut all_bodies = vec![Body::default(); filled_n];

    if is_root {
        // create input
        let mut masses = generate_random_bounded(args.n_bodies, 0f64, args.mass_max);
        masses.extend(repeat(0f64).take(extra_n));

        let mut all_positions =
            generate_random_bounded(args.n_bodies * 2, -args.pos_max, args.pos_max);
        all_positions.extend(repeat(0f64).take(extra_n * 2));

        let mut init_velocities =
            generate_random_bounded(args.n_bodies * 2, -args.velocity_max, args.velocity_max);
        init_velocities.extend(repeat(0f64).take(extra_n * 2));

        for (i, b) in all_bodies.iter_mut().enumerate() {
            b.id = i;
            b.mass = masses[i];
            b.position = all_positions[i * 2..(i + 1) * 2].try_into().unwrap();
            b.velocity = init_velocities[i * 2..(i + 1) * 2].try_into().unwrap();
        }
    }

    world
        .process_at_rank(ROOT_RANK as i32)
        .broadcast_into(&mut all_bodies);

    let mut local_bodies: Vec<Body> =
        all_bodies[rank * bodies_per_proc..(rank + 1) * bodies_per_proc].into();

    for _step in 0..args.n_steps {
        // initial tree root
        let bounds = get_bounds(
            &all_bodies
                .iter()
                .map(|b| b.position)
                .collect::<Vec<[f64; 2]>>(),
        );
        let size = f64::max(bounds[0][1] - bounds[0][0], bounds[1][1] - bounds[1][0]);
        let mut tree = TreeNode {
            center: [
                (bounds[0][1] + bounds[0][0]) / 2f64,
                (bounds[1][1] + bounds[1][0]) / 2f64,
            ],
            size,
            ..TreeNode::default()
        };

        barnes_hut(
            args.step_time,
            args.theta,
            &mut local_bodies,
            &mut tree,
            n_threads,
            &world,
        );

        let start_time = Instant::now();
        world.all_gather_into(&local_bodies, &mut all_bodies);
        unsafe { GATHER_DURATIONS.push(start_time.elapsed()) }
    }

    println!(
        "Rank {}: Avg subtree building duration: {:.2?}",
        rank,
        avg_duration(unsafe { &SUBTREE_DURATIONS })
    );
    println!(
        "Rank {}: Avg merge duration: {:.2?}",
        rank,
        avg_duration(unsafe { &MERGE_DURATIONS })
    );
    println!(
        "Rank {}: Avg force calc duration: {:.2?}",
        rank,
        avg_duration(unsafe { &CALC_DURATIONS })
    );
    println!(
        "Rank {}: Avg gather duration: {:.2?}",
        rank,
        avg_duration(unsafe { &GATHER_DURATIONS })
    );

    if is_root {
        println!("It took {:.2?}!", start_time.elapsed());
    }
}

fn avg_duration(durations: &Vec<Duration>) -> Duration {
    assert!(!durations.is_empty());

    let mut summed_durs = durations[0];

    durations
        .iter()
        .skip(1)
        .for_each(|d| summed_durs = summed_durs.checked_add(d.clone()).unwrap());

    summed_durs / durations.len() as u32
}

/// Calculate the new velocity of a body.
///
/// * `old_velocity`: Old velocity
/// * `force`: Current force on the body
/// * `mass`: Body's mass
/// * `timestep`: Step size of the time
fn calc_velocity(old_velocity: &[f64; 2], force: &[f64; 2], mass: f64, timestep: f64) -> [f64; 2] {
    let [v_x, v_y] = old_velocity;
    let [f_x, f_y] = force;
    [v_x + f_x / mass * timestep, v_y + f_y / mass * timestep]
}

/// Calculate the new position of a body.
///
/// * `velocity`: New velocity
/// * `old_position`: Old position
/// * `timestep`: Time step size
fn calc_position(velocity: &[f64; 2], old_position: &[f64; 2], timestep: f64) -> [f64; 2] {
    let [v_x, v_y] = velocity;
    let [x, y] = old_position;
    [x + v_x * timestep, y + v_y * timestep]
}
