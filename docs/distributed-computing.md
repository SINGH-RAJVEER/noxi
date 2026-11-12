# Distributed computing and fleet specification

## Status and product goal

noxi OS must be designed for distributed computing, including large fleets of identical machines and fleets that mix architectures, hardware, sites, and workloads. A fleet is a first-class configuration target, not a collection of hosts maintained by copied shell scripts. Users must be able to describe machines once, assign reusable roles, declare their relationships, and review how the OS will reach the desired state.

The goal is to make routine distributed operation require little manual coordination. Adding workers, balancing traffic, replacing a failed member, and rolling out an OS update should use the same configuration-first workflow. The OS must expose the resource limits and failure conditions that prevent convergence. It cannot manufacture physical capacity, make a network partition disappear, or promise uninterrupted writes without a working quorum.

This is a design specification, not implemented fleet support or a claim of unlimited scale. The [package-manager-first plan](../PLAN.md) remains unchanged. The single-machine OS must establish the identity, lifecycle, and operation contracts needed by this design; remote execution and distributed-computing releases follow their own qualification gates. Existing CppNix compatibility commands do not acquire fleet behavior.

The intended operator experience includes:

- Bring up a Kubernetes cluster through a declared kubeadm or K3s backend, with explicit control-plane and worker placement.
- Add eligible machines to a worker pool without duplicating per-host configuration or rebuilding identical closures separately.
- Describe an etcd quorum or a database primary with replicas using machine references, not hand-maintained peer lists on every host.
- Route API and application traffic through declared endpoints whose health checks and failover rules are visible in the plan.
- Preview a change across the exact affected machines, apply it in safe batches, and resume after coordinator or machine failure.
- Manage several independent clusters, edge sites, and non-Kubernetes services in one fleet without making them share a datastore or failure domain.

## Fleet model and authoritative state

| Declaration | Required meaning |
|---|---|
| Fleet | Administrative ownership, configuration source, trusted machine identities, and management policy |
| Machine | Stable identity, target system, hardware profile, management endpoints, capabilities, and failure-domain labels |
| Machine group | Reusable selection and configuration for a pool of machines; membership resolves to exact identities for a plan |
| Cluster | An independently identified runtime deployment, its selected backend, members, network, datastore ownership, and lifecycle |
| Service topology | Service-specific quorum, replication, dependency, placement, and transition requirements |
| Connection | A directed relationship with a purpose, endpoint, protocol, authentication reference, and network policy |
| Capacity policy | Desired capacity, eligible resources, minimum and maximum bounds, provisioning authority, and scale-in constraints |
| Fleet revision | Configuration source, inventory revision, and locked inputs that determine desired state |
| Fleet plan | Exact target identities and closures, required authority, transition ordering, health gates, and expected partial outcomes |

A hostname or IP address is not a machine identity. Enrollment must bind the inventory identity to authenticated machine credentials. Replacement, address changes, and identity rotation are explicit operations. A machine presenting a different identity at a known endpoint must not receive the old machine's generation or credentials.

The authoritative configuration may contain static inventory or import a recorded inventory snapshot from a provisioning system. Imported inventory must have a revision and provenance and become a tracked evaluation input. Evaluation must not query live DNS, cloud APIs, or etcd to decide which machines exist. Runtime discovery produces observations or a proposed inventory change; it cannot silently authorize a new target.

Observed health, active membership, leases, and operation journals are separate from desired state. A runtime registry may use an explicitly owned etcd deployment, but its expiring records do not replace the fleet configuration. Missing a heartbeat does not authorize deletion of a voting member. Kubernetes etcd, application coordination, and fleet-management state have separate ownership and access policies by default. Sharing a datastore requires a separately qualified isolation and recovery contract.

Each property must have one authority. Fleet configuration owns OS generations and declared cluster policy. Kubernetes controllers own Kubernetes runtime objects within their assigned scope. An autoscaler may own a worker pool's actual membership within configured bounds. The manager must not continually reset autoscaler-owned values to a literal replica count from an older plan. Imported Kubernetes manifests, an external GitOps controller, and native configuration must declare non-overlapping ownership or reject the conflict.

## Module interfaces and responsibility

Fleet support must add depth to the existing [product architecture](product.md#required-modules-and-seams), not move distributed-system algorithms into the GUI or into arbitrary user hooks.

| Module | Interface obligation | Owned behavior |
|---|---|---|
| Fleet planner | Accept a fleet revision, typed change, and recorded observations; return an immutable fleet plan | Inventory resolution, per-target evaluation, dependency ordering, authorization scope, capacity and availability checks |
| Topology adapter | Describe supported service transitions and their preconditions, execute authorized steps, and report verified postconditions | Backend-specific bootstrap, membership, drain, promotion, fencing, upgrade, and recovery rules |
| Machine executor | Accept a reviewed step bound to machine identity, closure, and operation identifier | Local authorization, activation serialization, durable step results, reboot and reconnect verification |
| Provisioning adapter | Resolve a bounded capacity request to admitted machines or a specific capacity failure | Infrastructure-specific creation, enrollment, readiness, removal, and quota handling |

NixBox coordinates plans and exposes inspection, preview, apply, observation, cancellation, and resume through one versioned operation interface. The noxi engine evaluates and realizes ordinary NixOS closures. NixOS activation applies each closure. Kubernetes schedules Pods; the fleet planner does not become another Pod scheduler. A remote Nix builder builds derivations and is not implicitly a Kubernetes worker.

Topology adapters may call pinned upstream tools such as kubeadm through typed execution steps. Their interfaces must define authority, retry behavior, durable state, and health checks. The result cannot be inferred solely from command exit status or human-readable output. There is no generic "run this shell command as root on every machine" substitute for these contracts.

A common plan format must preserve service-specific facts rather than reduce every distributed service to a replica count. Unsupported transitions return a diagnostic before mutation. A backend-specific setting may be exposed with provenance, but it cannot bypass quorum checks or secretly change another backend's defaults.

## Configuration contract and heterogeneous machines

Native fleet configuration is written in Nix. Equivalent inputs must resolve to the same inventory, relationships, transition policy, and per-machine `system.build.toplevel` derivation paths. The final schema and module interface remain implementation decisions; the examples below specify the information the interface must represent.

Configuration must support shared modules, named hardware profiles, machine groups, explicit per-machine overrides, and source provenance. Group membership and placement selectors must resolve deterministically. Unknown machine references, contradictory roles, duplicate identities, conflicting address ranges within a routing domain, unsupported backends, and unrepresentable placement requirements fail preview. Isolated clusters may reuse address ranges, but connecting their networks requires an explicit plan that resolves any overlap. An override must show which shared definition it changes.

Each machine evaluates for its own target system and hardware. A homogeneous group may reuse a built closure only when its actual derivation identity matches. A heterogeneous group may share policy but still needs distinct kernels, drivers, boot artifacts, and packages. Native builders, cross-compilation, and emulation are separate declared capabilities; an x86_64 binary must never be sent to an aarch64 target merely because both belong to the same role.

Placement must account for architecture, memory, storage, accelerators, device access, and relevant network capabilities. Kubernetes labels, taints, tolerations, affinity, topology spread, and resource requests must express those constraints where Kubernetes owns placement. GPU drivers, device plugins, CNI and CSI implementations, and application images need their own architecture qualification. A multi-architecture image index selects matching images; it does not translate executables.

The planner must identify failure domains such as host, rack, power supply, site, and zone. Three members on one physical host do not satisfy a requirement to tolerate loss of that host. Shared power, storage, or network dependencies must remain visible even when the machines have different names. Geography is a placement constraint, not proof that a latency-sensitive quorum will work over a WAN.

Secret declarations contain references and access requirements, never token or key values. Machines retrieve credentials through an authorized runtime path outside Nix store objects, derivations, images, and logs. Preview can validate the reference and required authority without disclosing the secret.

## Connected-machine configuration examples

These are illustrative desired-state records, not runnable modules, established NixOS options, or qualified deployment recipes. Addresses use documentation ranges. Profile definitions, credential delivery, networking, and package versions would be supplied by the selected fleet inputs. The example describes a mixed-architecture Kubernetes cluster with dedicated external etcd and a separate PostgreSQL primary/replica service.

```nix
{
	id = "research";
	machines = {
		cp-a = { profile = "server-x86"; zone = "a"; address = "192.0.2.11"; };
		cp-b = { profile = "server-x86"; zone = "b"; address = "192.0.2.12"; };
		cp-c = { profile = "server-x86"; zone = "c"; address = "192.0.2.13"; };
		kv-a = { profile = "server-x86"; zone = "a"; address = "192.0.2.21"; };
		kv-b = { profile = "server-x86"; zone = "b"; address = "192.0.2.22"; };
		kv-c = { profile = "server-x86"; zone = "c"; address = "192.0.2.23"; };
		worker-x86 = { profile = "worker-x86"; zone = "a"; address = "192.0.2.31"; };
		worker-arm = { profile = "worker-arm"; zone = "b"; address = "192.0.2.32"; };
		db-a = { profile = "server-x86"; zone = "a"; address = "192.0.2.41"; };
		db-b = { profile = "server-x86"; zone = "b"; address = "192.0.2.42"; };
		db-c = { profile = "server-x86"; zone = "c"; address = "192.0.2.43"; };
	};
	profiles = {
		server-x86 = { system = "x86_64-linux"; role = "server"; };
		worker-x86 = { system = "x86_64-linux"; role = "worker"; };
		worker-arm = { system = "aarch64-linux"; role = "worker"; };
	};
	topologies = {
		k8s-state = {
			kind = "quorum"; adapter = "etcd";
			voters = [ "kv-a" "kv-b" "kv-c" ];
			failureDomain = "zone"; ownership = "kubernetes/main";
		};
		analytics = {
			kind = "primary-replica"; adapter = "postgresql";
			primary = "db-a"; replicas = [ "db-b" "db-c" ];
			replication = "asynchronous"; promotion = "manual-fenced";
			writeEndpoint = "analytics.example.test";
		};
	};
	kubernetes.main = {
		bootstrap = "kubeadm";
		controlPlane = [ "cp-a" "cp-b" "cp-c" ];
		workers = [ "worker-x86" "worker-arm" ];
		datastore = { kind = "external-etcd"; topology = "k8s-state"; };
		apiEndpoint = "api.example.test:6443";
		podCIDR = "10.244.0.0/16"; serviceCIDR = "10.96.0.0/16";
	};
	rollout = { workerCanaries = 1; workerBatchSize = 1; haltOnFailedHealth = true; };
}
```

The planner derives peer addresses and directed connections from stable machine references. It must show the resulting etcd peer/client connections, control-plane datastore access, worker API access, and database replication connections before apply. Those connections need explicit network and credential policy; membership is not permission to expose a port publicly.

The API endpoint needs a declared load-balancing implementation. The database endpoint needs primary-aware routing and a fencing implementation before promotion is permitted. Asynchronous replication can lose acknowledged writes during failover, so the database deployment also needs an accepted data-loss policy and backup/restore procedure. Preview must reject the abbreviated example until these requirements and qualified versions are supplied.

For a new, separately identified K3s cluster, the corresponding backend record can instead describe embedded etcd:

```nix
{
	id = "edge";
	bootstrap = "k3s";
	servers = [ "cp-a" "cp-b" "cp-c" ];
	agents = [ "worker-x86" "worker-arm" ];
	datastore = { kind = "embedded-etcd"; };
	registrationEndpoint = "edge.example.test:6443";
}
```

This alternative reuses the illustrative inventory shape, not machines already assigned to the kubeadm cluster. Changing a backend field is not an in-place kubeadm-to-K3s migration. Changing datastore kind is not a data migration either. Both require explicit supported procedures or a new cluster.

## Kubernetes backends and lifecycle

kubeadm and K3s are distinct planned backends, not aliases. The first fleet release that advertises both must qualify both. Each backend must declare its exact version, architecture support, container runtime, datastore, network implementation, storage integrations, and supported upgrade path. A later backend may join only after it satisfies the same lifecycle contract.

| Backend | Required topology distinctions |
|---|---|
| kubeadm | Stacked control-plane/etcd or dedicated external etcd; explicit control-plane and worker roles; stable API endpoint |
| K3s | Single-server SQLite, HA embedded etcd, or a supported external datastore; explicit server and agent roles; stable registration/API endpoint |

A kubeadm HA plan must account for the coupling between control-plane and etcd failures in stacked topology. External etcd has its own membership and rollout policy. The documented production HA baseline uses at least three control-plane hosts, and external etcd adds at least three separate etcd hosts. Smaller configurations must be identified as having reduced availability, not silently described as HA. [kubeadm HA topology](https://kubernetes.io/docs/setup/production-environment/tools/kubeadm/ha-topology/)

K3s SQLite is a single-server datastore, not a multi-server HA option. Embedded-etcd HA uses three or more servers with an odd steady-state server count. Cluster-critical server settings must agree. External etcd and supported SQL datastores need backend-specific version and configuration checks; arbitrary SQL multi-primary replication is not automatically supported. Existing initialized state must be inspected before datastore settings change. [K3s embedded etcd](https://docs.k3s.io/datastore/ha-embedded), [K3s datastore options](https://docs.k3s.io/datastore)

Each adapter must implement or explicitly reject these operations:

1. Validate the intended topology, ports, DNS, time synchronization, address ranges, cgroups, runtime, CNI, storage prerequisites, and certificates before bootstrap.
2. Establish one cluster for the declared identity. Join subsequent members using backend- and role-appropriate credentials with the shortest supported lifetime and verified endpoints. A retry must inspect existing state rather than create a second cluster.
3. Verify API, datastore, node, network, and required add-on readiness. A running systemd unit does not prove a usable cluster.
4. Add workers and control-plane members through their distinct join paths. Track each machine's readiness and actual runtime membership.
5. Drain, upgrade, reboot, and return members to service in the backend's supported order. Honor Pod disruption budgets and service-specific availability constraints; an expired timeout is not permission to force eviction.
6. Remove or replace a member through an explicit plan that handles datastore membership, workload evacuation, local state, fleet-credential revocation, and effective denial of further cluster access.
7. Rotate certificates and enrollment credentials without leaking them into generated closures or logs.
8. Back up and recover the datastore, including ownership of restore approval and validation of cluster state after restore.

K3s expiring bootstrap tokens support agent enrollment; server joins require the persistent server token. Deliver that token through the protected runtime path and coordinate its rotation. The server token also encrypts datastore bootstrap data, so protected backups must retain the token corresponding to each retained snapshot, including older tokens needed after rotation. A generic expiring-token abstraction cannot erase this recovery dependency. [K3s token lifecycle](https://docs.k3s.io/cli/token)

Each backend must state how retirement removes effective access and prevents renewal or re-enrollment. Kubernetes client certificates do not support individual revocation; deleting a Node, CSR, or bootstrap token does not invalidate an issued certificate. Qualified authorization, trust changes, or enforced isolation through credential expiry must establish the intended denial. Keep retirement blocked or the machine explicitly quarantined until that condition is verified. Fleet-management credential revocation and Kubernetes client-access denial are distinct results. [Kubernetes client certificates](https://kubernetes.io/docs/reference/access-authn-authz/authentication/#x509-client-certificates-k8s)

Version-skew rules are plan constraints, not warnings to ignore. Pin the backend's supported skew and upgrade rules; they may be stricter than Kubernetes's general policy. Do not skip Kubernetes minor versions for API-server upgrades, and drain workloads before a minor kubelet upgrade. A generation rollback that selects an incompatible Kubernetes or datastore binary must be rejected or routed through a supported recovery procedure. [Kubernetes version-skew policy](https://kubernetes.io/releases/version-skew-policy/)

Declarative OS configuration owns installed versions and service configuration. Kubernetes owns runtime scheduling and objects assigned to it. Bootstrap and membership state stay in explicitly managed runtime storage. Rebuilding a host must not reset cluster identity, regenerate the whole PKI, or erase the datastore.

## Networking, discovery, and load balancing

Networking must be described as a connectivity contract, including management traffic, cluster API traffic, Pod and Service networks, storage traffic, and public or private workload endpoints. The contract records IPv4/IPv6 support, routing or overlay requirements, MTU, DNS, firewall policy, encryption, and site connectivity. These properties require separate qualification per chosen network implementation.

The OS must distinguish four different concerns:

| Concern | Configuration and ownership |
|---|---|
| Kubernetes API availability | Stable endpoint, control-plane health checks, backend membership, and load-balancer failover |
| Workload ingress and service traffic | Service/Ingress or Gateway declarations, exposure policy, L4/L7 routing, TLS, health checks, and connection draining |
| Stateful-service routing | Primary-only writes, allowed replica reads, lag and consistency policy, fencing, and endpoint changes after authorized promotion |
| Runtime discovery | Authenticated names and registrations, TTL/lease behavior, stale endpoints, and isolation between clusters |

Cloud load balancers, bare-metal address advertisement, and host-level proxies are alternative adapters with different prerequisites. A declaration must identify who owns addresses and routes and how a failed load balancer is replaced. A single proxy machine must not be presented as a highly available endpoint. Naming a Service `LoadBalancer` cannot imply that bare-metal infrastructure already supplies a working load balancer.

Load balancing must consider backend health and capacity and must expose its policy. Readiness checks, traffic removal, workload drain, and machine shutdown happen in the required order. Session affinity and long-lived connections need stated behavior; traffic cannot always move without interruption. Persistent volumes do not become portable because traffic was rerouted.

Connections between sites require explicit trust, routing, and latency assumptions. Default to independent clusters for sites whose connectivity cannot meet the backend's availability requirements. Cross-site replication and failover need a service-specific data-loss and consistency contract. No automatic split-brain "repair" may join incompatible histories or bypass fencing.

## Capacity, placement, and scaling

Scaling must be a declarative change with a preview and observable completion conditions. Separate workload replicas, available worker capacity, and control-plane/datastore membership. Increasing one does not safely increase the others.

- Workload scaling uses the scheduler and autoscaling controllers appropriate to the workload. Kubernetes HPA changes workload replica counts; resource requests, metrics availability, and configured limits must be visible.
- Worker scaling selects admitted spare machines or provisions machines through a qualified adapter. Bare-metal capacity needs an explicit enrollment and provisioning path, such as approved spare nodes or an integrated network-boot workflow. If none exists, report insufficient capacity rather than claim nodes were created.
- Control-plane and quorum scaling uses explicit membership plans. A worker autoscaler must never remove etcd voters or control-plane members as interchangeable idle nodes.
- Scale-in first verifies placement, storage attachment, disruption budgets, and local-data retention. It drains and removes membership, revokes fleet credentials, and establishes backend-specific denial of further service access before releasing infrastructure. Failure leaves the machine accounted for and blocks destructive cleanup.

Capacity policy must declare pool eligibility, desired or autoscaler-owned capacity, minimum and maximum size, headroom, quotas, cost or power constraints where applicable, stabilization periods, and scale-in protections. An authorized autoscaler may execute bounded preapproved transitions; changes outside those limits require a new reviewed plan. Manual apply and autoscaling must share conflict checks and durable operation records.

Node autoscaling needs an infrastructure integration and uses workload resource requests and scheduling constraints. It is not simply a CPU-usage trigger. Publish unsupported placement, unavailable images, unsatisfied persistent-volume requirements, exhausted quota, and hardware shortages as distinct reasons for non-convergence. [Kubernetes node autoscaling](https://kubernetes.io/docs/concepts/cluster-administration/node-autoscaling/), [multi-architecture images](https://kubernetes.io/docs/concepts/containers/images/#multi-architecture-images-with-image-indexes)

For a fixed homogeneous pool, changing worker capacity from 20 to 24 should resolve four eligible machines, show their identities and closures, and enroll them without rewriting the other twenty declarations. For a heterogeneous pool, capacity must be expressed by capabilities or per-pool bounds rather than treating a low-power ARM machine and a GPU server as equal capacity. Both cases need a plan for what happens when only three of the four machines become ready.

## Quorum and primary/replica service topologies

### Quorum membership

An etcd declaration describes cluster identity, voting members, non-voting learners where supported, peer and client endpoints, TLS references, persistent storage, failure domains, and backup ownership. etcd elects its own leader. Configuration must not declare a permanently privileged etcd primary.

For `N` voting members, quorum is `floor(N / 2) + 1`. Three voters tolerate one unavailable member; five tolerate two. Two voters do not tolerate losing one. The planner must use observed healthy membership and quorum, not only the desired member count, before changing membership or taking a host down.

Reconfiguration requires a working majority and sequential, backend-supported membership changes. Prefer adding a learner, verifying catch-up, and then promoting it when the qualified version supports that sequence. The exact order of replacement depends on current membership and health; the adapter must prove each intermediate state is safe instead of prescribing a universal add-before-remove rule. Do not disable etcd's strict reconfiguration checks to make a plan proceed. [etcd runtime reconfiguration](https://etcd.io/docs/v3.6/op-guide/runtime-configuration/)

Loss of majority is a disaster-recovery condition, not permission to keep deleting missing peers. Snapshot restoration establishes a new logical cluster and requires the backend's recovery procedure, validation, and controlled client reconnection. Kubernetes recovery must account for etcd revision and watch-cache behavior. The saved OS generation alone is not a backup of quorum state. [etcd disaster recovery](https://etcd.io/docs/v3.6/op-guide/recovery/)

### Primary and replicas

A primary/replica declaration must identify the service adapter, initial primary or supported election authority, eligible replicas, replication mode, write and read endpoints, promotion rules, fencing mechanism, backup policy, accepted data-loss bounds, and recovery objectives. These are relationships, not just labels on machines.

The service adapter must specify what "healthy", "caught up", and "safe to promote" mean for that service. Synchronous and asynchronous replication have different write-availability and data-loss behavior. Read replicas can return stale data. A generic replica count cannot establish either consistency or safe failover.

Promotion requires authority and evidence that the old primary cannot continue accepting writes. A failed heartbeat alone does not supply that evidence. If fencing cannot be established or replication lag exceeds the accepted policy, stop and explain why. Rejoining an old primary may require resynchronization or rebuilding its data; do not automatically reconnect divergent histories.

Initial primary placement belongs in desired state. An authorized runtime election may change the current primary without rewriting the fleet source on every election. The planner must distinguish allowed role changes from an explicit operator-requested primary relocation, and record both actual and intended roles.

The first adapter qualification must be service-specific, such as a supported PostgreSQL version and failover method. Redis replication, message brokers, distributed filesystems, and other databases cannot inherit a PostgreSQL failover claim. Unsupported services may still run as ordinary NixOS services, but the manager must not advertise automated topology management for them.

## Fleet plan, rollout, and recovery

A fleet plan extends the [declarative configuration workflow](configuration-workflow.md) and retains its per-machine NixOS activation semantics. There is no fleet-wide atomic switch or single fleet generation.

1. Record the fleet revision, exact target identities, source ownership, current generations, runtime membership, and health observations. Expand selectors now, not again after approval. State observation age and refresh requirements.
2. Evaluate the candidate for every target system. Return exact closures, connection and membership changes, unsupported hardware, capacity failures, data-migration effects, and required authority. Build and verify the required closures before mutation according to the approved staging policy.
3. Construct a dependency graph and batches. Apply canaries where meaningful, then health-gated batches with per-topology limits. A generic `maxUnavailable` value cannot override a stricter quorum, primary, storage, or Pod disruption constraint.
4. Recheck source and inventory revisions, machine identity, activation state, relevant runtime membership, and authorization. Persist the desired fleet revision through the source journal and verify its evaluated closures. Stop stale or mismatched work before activation.
5. Execute steps with target-bound idempotency identifiers and durable preconditions and results. Each local executor serializes generation mutations. Membership changes and promotions need their own protected coordination. All drains, activations, and reboots that consume service availability also acquire topology-wide availability reservations shared across concurrent plans. A per-machine lock alone cannot protect cross-machine topology or availability.
6. Verify service health, not just SSH availability or successful activation. After reboot, verify machine identity, running generation, boot default, membership, workload readiness, and traffic behavior as applicable before admitting the next batch.
7. Report completed, failed, unreachable, blocked, and not-started targets separately. Keep the desired fleet revision and actual per-machine generations visible when only part of the fleet converged. Offer an evidence-based resume, replan, or service-specific recovery action.

Rolling out a kernel update to Kubernetes workers normally removes traffic where needed, drains workloads, activates the generation, reboots if required, verifies the node, and returns it to scheduling. A control-plane update also checks API and datastore availability. A database update additionally checks replication and data-format compatibility. These are different transition plans even if all begin with the same OS package change.

Recheck live health before each disruptive step and hold its availability reservation until verified recovery. Two independent plans must not each reboot a different voter in a three-voter quorum because both saw the other voters healthy. Unknown or unreachable state cannot silently free a reservation; reconciliation or an explicitly authorized recovery decision must resolve it first.

Coordinator leadership and retries must not produce two active owners of the same unsafe transition. An executor must reject stale authority using a backend-qualified fencing mechanism, such as monotonic epochs enforced at the mutation point. An expiring lease alone does not stop a disconnected former coordinator. On restart, inspect durable results and observed postconditions before retrying an interrupted external action. Do not promise exactly-once execution where the backend cannot establish it.

Cancellation stops scheduling new work and names in-flight steps that must finish or require recovery. An unreachable machine has unknown state, not a successful rollback. A disconnected site may continue workloads according to its local policy, but cannot accept conflicting fleet mutations without a valid authority contract. On reconnect, compare identities and state before resuming.

Generation rollback and data recovery remain separate. Keep prior bootable closures and recorded configuration revisions, but reject binary downgrades incompatible with already migrated data. Recovery plans must identify snapshot age, expected data loss, restore authority, dependent services, and the checks needed before traffic resumes. Restoring a fleet coordinator journal must not overwrite newer cluster state.

## Large-fleet execution and visibility

The implementation must avoid making ordinary changes depend on an unbounded all-host connection fan-out or on one workstation staying online. Bound evaluation, build, transfer, and activation concurrency separately. Partition work by dependency, failure domain, and target capability while preserving one reviewed plan and attributable child operations.

Deduplicate realization by derivation identity, distribute signed cache artifacts through declared trusted caches, and verify closures on each target. A successful central build does not prove that every machine received a complete closure. Retain roots for deployed and recovery generations until the stated policy permits collection. Cache outages, transfer throttling, and a partially unavailable site must have explicit outcomes.

The plan and event interfaces must support paginated inventory, targeted refresh, durable event cursors, backpressure, bounded log retention, and interrupted transfer resumption where supported. A status request must not copy every full build log or force reevaluation of every unaffected machine. Reuse evaluation results only when their tracked inputs and relevant dependencies match.

Observability must distinguish desired revision, observed revision, running generation, boot default, machine reachability, service readiness, replication lag, quorum availability, pending capacity, and drift. Fleet operations use the [logging contract](logging.md), with machine, cluster, topology, batch, and parent-operation correlation. Event streams do not invent a global causal order across independent machines.

Administrative access must be scoped by fleet, machine group, cluster, topology, and action. Enrollment credentials, mutation authority, cache trust, and service credentials are separate permissions. Replay, compromised machine identity, stale plans, and recovery access need explicit checks and auditable results. Ordinary fleet management must work headlessly; a desktop session cannot be its control plane.

## Distributed-computing possibilities

The model should support these uses without pretending their runtimes have the same scheduling or recovery semantics:

| Use | What the OS can make declarative | Qualification still required |
|---|---|---|
| Application platforms | Kubernetes pools, ingress, storage prerequisites, update and disruption policy | Chosen runtime, CNI/CSI, images, traffic failover, and persistent-data behavior |
| Scientific and batch computing | CPU/GPU pools, drivers, RDMA-capable networking, shared storage, scheduler services | Scheduler-specific allocation, job checkpointing, fabric and device support |
| Data and stream processing | Quorum services, primary/replica storage, broker placement, capacity and backup policy | Service-specific consistency, rebalancing, migration, and recovery |
| Distributed Nix builds | Builder pools, architecture capabilities, cache placement, credentials and quotas | Reference-compatible build scheduling and cache trust; isolation from production workloads |
| Edge and intermittent sites | Local clusters, bounded local authority, cached generations, cross-site replication | Partition policy, reconnection, accepted data loss, and site recovery |
| Mixed personal or institutional fleets | Shared profiles, hardware overrides, spare-machine admission, reproducible role changes | Hardware support, power/network reliability, and ownership permissions |

Kubernetes is the first orchestration integration, not a requirement for every machine or distributed workload. A fleet may mix Kubernetes nodes, dedicated storage hosts, build workers, and independently managed services. Later scheduler or service adapters should reuse inventory, plans, enrollment, and diagnostics instead of introducing a competing source of desired state.

The OS does not make several machines share one kernel, one transparent memory space, or an automatically distributed application. Applications still choose a distributed runtime and data model. OS abstractions must remove repetitive deployment work while keeping those choices and their costs visible.

## Acceptance cases and release evidence

Fleet qualification is separate from the CppNix [compatibility matrix](compatibility-matrix.md) and the [single-machine OS gates](verification.md#release-gates). Record exact noxi OS artifacts, locked inputs, Kubernetes/K3s/etcd and service versions, architectures, provisioning and network adapters, topology, machine count, workload, test commands, and results. All behavior specified here is currently unrun.

Required acceptance cases include:

- Equivalent fleet inputs resolve identical identities, service relationships, plans, and per-target derivations. Live discovery cannot change a pure evaluation.
- A homogeneous pool reuses only identical closures; a mixed x86_64/aarch64 pool gets correct per-target closures and compatible workload images. Unsupported add-ons or hardware block preview rather than fail after rollout.
- kubeadm HA bootstraps and recovers using both advertised datastore topologies. K3s single-server, embedded-etcd HA, and each advertised external datastore are qualified separately. Repeated bootstrap/join requests do not duplicate cluster identity or membership.
- API and workload load balancers survive the advertised backend and balancer failures. Tests verify traffic and connection-drain behavior, not only process health.
- Worker capacity expands and contracts through a declared provisioning path. Exhausted capacity, partial enrollment, blocked disruption budgets, unavailable volumes, and autoscaler/manual-operation conflicts remain visible and non-destructive.
- An etcd member is replaced under healthy and degraded conditions without unsafe intermediate quorum. Loss of majority stops ordinary reconfiguration and requires tested snapshot recovery. No OS rollback falsely reports restored datastore contents.
- The qualified primary/replica adapter tests replica lag, old-primary fencing, denied unsafe promotion, endpoint updates, divergent-primary rejoin, backup restore, and the declared data-loss policy.
- A rollout handles stale inventory, external source edits, machine-identity replacement, conflicting coordinators, concurrent plans consuming the same availability budget, coordinator restart, lost acknowledgements, machine reboot, cancellation, and unreachable hosts without replaying an unsafe transition.
- Cross-site partitions preserve the declared authority policy and report unknown state. Reconnection cannot apply a plan to a replacement machine or overwrite newer runtime membership.
- CLI, TUI, and GUI produce the same target set, authorization decisions, plan, events, and partial results; the full workflow works without a desktop.
- Secret delivery, certificate rotation, snapshot-associated K3s token recovery, fleet-credential revocation, effective denial of retired Kubernetes access, replay rejection, untrusted caches, and compromised enrollment attempts pass security tests with no secret-bearing store inputs or logs.

Scale evidence must be numeric. Exercise small multi-machine functional tests first, then representative fleet sizes of 100, 1,000, and 10,000 machines as qualification targets. These numbers are planned test tiers, not current support claims. Measure plan latency, peak coordinator/executor memory, evaluation reuse, cache bandwidth, event volume, rollout convergence, and recovery time under stated workloads and failures. Simulator-only results must be labeled; they do not establish real-machine boot, provisioning, or traffic behavior.

Fleet size is not single-cluster size. Record the number of independent clusters, nodes and workloads per cluster, datastore members and data volume, and the distribution of sites and hardware. Respect the selected Kubernetes/backend's qualified per-cluster limits. A 10,000-machine fleet distributed across independent clusters cannot be presented as evidence that one Kubernetes cluster supports 10,000 nodes.

Before claiming a supported fleet size, publish measured results and release-specific pass/fail budgets for its representative hardware and network. A smaller first fleet release may advertise only its tested tier. "Effortless scaling" means a bounded configuration change and verified convergence under those published limits, not unqualified support for any number of machines.

## Delivery sequence and decisions

1. Design the single-machine OS's typed lifecycle, source journal, identity, and authorization contracts so fleet operations can use them without replacing NixOS generation semantics.
2. Qualify authenticated remote execution, replay protection, conflict detection, durable recovery, and headless enrollment after the single-machine OS foundation. No remote mutation ships before these gates pass.
3. Qualify bounded fleet rollouts, connected-machine descriptions, kubeadm and K3s lifecycle adapters with their required networking and load balancing, and the first explicitly supported quorum and primary/replica adapters.
4. Expand provisioning, load-balancing, autoscaling, heterogeneous targets, multi-site policies, and measured large-fleet tiers. Expand support only with new evidence.

The [root plan](../PLAN.md) remains the only product plan, and the [execution checklist](../TODO.md) tracks this work. Before the first fleet release, settle the typed configuration schema, transport and enrollment method, coordinator authority/fencing, initial supported versions and architectures, service-adapter scope, network and storage implementations, provisioning integrations, backup ownership, and scale budgets. Document each selected support combination and each unsupported transition. None of these decisions may weaken the package-manager compatibility gate.
