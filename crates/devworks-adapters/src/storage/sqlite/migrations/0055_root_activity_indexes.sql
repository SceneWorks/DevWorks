-- Root activity: whether a root may still be running reads only that root's unsettled process
-- trees and its agent containers that may still run, instead of every unsettled row.
CREATE INDEX owned_process_root_unsettled ON owned_processes(root_id,id)
    WHERE root_id IS NOT NULL AND state NOT IN ('Reaped','Failed');
CREATE INDEX owned_containers_root_agent_work ON owned_containers(root_id,id)
    WHERE root_id IS NOT NULL AND owner_kind='Agent'
      AND state IN ('Creating','Starting','Running','Stopping','Uncertain');
