"""Real local Git effects with scripted PR/check/review metadata; no GitHub writes or inference."""
from copy import deepcopy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
ROOT=Path(__file__).resolve().parents[2];CHECKS=[]
def check(name,condition):
    if not condition:raise AssertionError(name)
    CHECKS.append(name)
class Repo:
    def __init__(self,root,name):
        self.name=name;self.remote=root/(name+'.git');self.local=root/(name+'-owner');self.templates=root/'empty-template';self.templates.mkdir(exist_ok=True)
        self.env={**os.environ,'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':os.devnull,'GIT_TERMINAL_PROMPT':'0'}
        self.git(None,'init','--bare','--initial-branch=main','--template='+str(self.templates),str(self.remote))
        self.git(None,'init','--initial-branch=codex/bootstrap','--template='+str(self.templates),str(self.local))
        self.git(self.local,'config','user.name','DevWorks Fixture');self.git(self.local,'config','user.email','fixture@invalid.example')
        self.git(self.local,'remote','add','origin',str(self.remote))
        (self.local/'base.txt').write_text('base\n');self.git(self.local,'add','base.txt');self.git(self.local,'commit','-m','Fixture base');self.git(self.local,'push','origin','HEAD:refs/heads/main')
        self.git(self.local,'switch','-c','codex/feature');(self.local/'feature.txt').write_text('feature\n');self.git(self.local,'add','feature.txt');self.git(self.local,'commit','-m','Fixture feature');self.git(self.local,'push','origin','HEAD:refs/heads/codex/feature')
        self.expected=self.oid('refs/heads/codex/feature');self.base=self.oid('refs/heads/main');self.observed_head=self.expected;self.checks=True;self.reviews=True;self.merges=0;self.state='Ready'
    def git(self,cwd,*args,check=True,input=None):
        command=['git','-c','core.hooksPath='+str(self.templates)]
        if cwd:command+=['-C',str(cwd)]
        r=subprocess.run(command+list(args),env=self.env,capture_output=True,text=True,input=input)
        if check and r.returncode:raise RuntimeError(r.stderr)
        return r
    def oid(self,ref):return self.git(self.remote,'rev-parse',ref).stdout.strip()
    def ready(self):return self.checks and self.reviews and self.observed_head==self.expected and self.oid('refs/heads/codex/feature')==self.expected
    def advance_feature(self):
        (self.local/'feature.txt').write_text('changed feature\n');self.git(self.local,'add','feature.txt');self.git(self.local,'commit','-m','Changed head');self.git(self.local,'push','origin','HEAD:refs/heads/codex/feature')
    def merge(self,lose_ack=False):
        if not self.ready():return 'Blocked'
        self.git(self.local,'fetch','origin');self.git(self.local,'switch','-C','codex/integration','origin/main');self.git(self.local,'merge','--no-ff','--no-edit',self.expected);self.git(self.local,'push','origin','HEAD:refs/heads/main');self.merges+=1
        self.state='OutcomeUnknown' if lose_ack else 'Merged';return self.state
    def recover(self):
        # This fixture uses ordinary merge commits. Squash/rebase need provider evidence.
        result=self.git(self.remote,'merge-base','--is-ancestor',self.expected,'refs/heads/main',check=False)
        if result.returncode==0:self.state='Merged'
        return self.state
    def task_worktree(self,root,suffix):
        self.worktree=root/(self.name+'-'+suffix);self.branch='refs/heads/codex/'+suffix
        self.git(self.local,'worktree','add','-b','codex/'+suffix,str(self.worktree),self.expected);return self.worktree
    def cleanup(self,expected,active=False,owned=True,race=None):
        if not owned or active:return 'Held'
        if self.git(self.worktree,'status','--porcelain').stdout:return 'Held'
        if self.git(self.local,'rev-parse',self.branch).stdout.strip()!=expected:return 'Held'
        if self.git(self.remote,'merge-base','--is-ancestor',expected,'refs/heads/main',check=False).returncode:return 'Held'
        self.git(self.local,'worktree','remove',str(self.worktree))
        if race:race()
        r=self.git(self.local,'update-ref','-d',self.branch,expected,check=False)
        return 'Removed' if r.returncode==0 else 'WorktreeRemovedBranchRetained'
class Delivery:
    def __init__(self,repos,dependencies):self.repos={r.name:r for r in repos};self.dependencies=dependencies;self.prepared=False
    def prepare(self):
        if any(a not in self.repos or b not in self.repos or a==b for a,b in self.dependencies):return False
        def visit(n,active,done):
            if n in active:return False
            if n in done:return True
            active.add(n)
            for a,b in self.dependencies:
                if a==n and not visit(b,active,done):return False
            active.remove(n);done.add(n);return True
        if not all(visit(n,set(),set()) for n in self.repos):return False
        self.prepared=all(r.ready() for r in self.repos.values());return self.prepared
    def merge(self,name,lose_ack=False):
        r=self.repos[name]
        if r.state=='Merged':return 'AlreadyMerged'
        if not self.prepared or any(self.repos[a].state!='Merged' for a,b in self.dependencies if b==name):return 'Blocked'
        return r.merge(lose_ack)
    def complete(self):return all(r.state=='Merged' for r in self.repos.values())

def run():
 with tempfile.TemporaryDirectory(prefix='devworks-delivery-') as temporary:
    root=Path(temporary);a=Repo(root,'a');b=Repo(root,'b');delivery=Delivery([a,b],[('a','b')])
    b.reviews=False;check('missing_review_prevents_first_merge',not delivery.prepare() and delivery.merge('a')=='Blocked' and a.oid('refs/heads/main')==a.base)
    b.reviews=True;check('all_head_evidence_prepares_barrier',delivery.prepare());check('dependency_order_blocks_later_repository',delivery.merge('b')=='Blocked')
    check('lost_ack_after_real_merge_is_unknown',delivery.merge('a',True)=='OutcomeUnknown' and not delivery.complete())
    a_oid=a.oid('refs/heads/main');check('recovery_observes_actual_merge_without_resubmitting',a.recover()=='Merged' and delivery.merge('a')=='AlreadyMerged' and a.merges==1)
    b.advance_feature();check('changed_head_invalidates_remaining_delivery',delivery.merge('b')=='Blocked' and b.oid('refs/heads/main')==b.base and not delivery.complete())
    check('partial_delivery_preserves_earlier_merge_without_revert',a.oid('refs/heads/main')==a_oid and a.state=='Merged')
    b.expected=b.oid('refs/heads/codex/feature');b.observed_head=b.expected;b.checks=False;check('new_head_requires_new_checks',delivery.merge('b')=='Blocked')
    b.checks=True;check('refreshed_evidence_allows_remaining_merge',delivery.merge('b')=='Merged' and delivery.complete())
    cyclic=Delivery([a,b],[('a','b'),('b','a')]);check('dependency_cycle_rejected_before_effects',not cyclic.prepare())
    a.task_worktree(root,'clean-owned');check('active_writer_holds_cleanup',a.cleanup(a.expected,active=True)=='Held' and a.worktree.exists())
    check('unowned_path_never_removed',a.cleanup(a.expected,owned=False)=='Held' and a.worktree.exists())
    (a.worktree/'new.txt').write_text('keep untracked work\n');check('untracked_changes_hold_cleanup',a.cleanup(a.expected)=='Held' and a.worktree.exists());(a.worktree/'new.txt').unlink()
    check('verified_owned_integrated_clean_worktree_removed',a.cleanup(a.expected)=='Removed' and not a.worktree.exists() and a.git(a.local,'show-ref','--verify',a.branch,check=False).returncode!=0)
    b.task_worktree(root,'cleanup-race');old=b.expected
    tree=b.git(b.local,'rev-parse',old+'^{tree}').stdout.strip();new=b.git(b.local,'commit-tree',tree,'-p',old,input='Retain concurrent work\n').stdout.strip()
    def advance_branch():b.git(b.local,'update-ref',b.branch,new,old)
    outcome=b.cleanup(old,race=advance_branch)
    check('branch_cas_preserves_concurrent_commit_after_worktree_removal',outcome=='WorktreeRemovedBranchRetained' and b.git(b.local,'rev-parse',b.branch).stdout.strip()==new)
    # A queue/job receipt is intentionally not considered merged by the delivery state model.
    b.state='Queued';check('queued_merge_does_not_complete_feature',not delivery.complete());b.state='OutcomeUnknown';check('unknown_merge_requires_observation',not delivery.complete())
    version=a.git(None,'--version').stdout.strip()
 report={'date':'2026-11-01','status':'Real local Git effects with scripted PR/check/review and queue states; no GitHub API qualification','gitVersion':version,'passed':len(CHECKS),'checks':CHECKS,'noInference':True,'noGitHubWrites':True,'allRepositoriesOwnedTemporary':True,'allTemporaryFixturesRemoved':True,'limitations':['The fixture uses ordinary merge commits; squash/rebase recovery requires provider evidence.','Queue, PR, check and review state are scripted rather than a live GitHub integration.','No atomic cross-repository transaction, target-base CAS, production authority or OS path-race guarantee is proved.']}
 out=ROOT/'docs/architecture/validation/data-delivery-contracts/git-delivery-results.json';out.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps({'passed':len(CHECKS),'gitVersion':version,'temporaryFixturesRemoved':True}))
if __name__=='__main__':run()
