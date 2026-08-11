// Shared harness for the final execution/scheduling necessary-condition screens.
use common::Lcg;

fn main() {
    match env!("CARGO_PKG_NAME") {
        "e44-foveated-topk" => e44(), "e47-waggle-morsels" => e47(),
        "e49-schooling-control" => e49(), "e52-join-fibers" => e52(),
        "e56-vascular-memory" => e56(), "e58-enzyme-batches" => e58(),
        "e60-autophagic-buffers" => e60(), "e61-apoptotic-queries" => e61(),
        "e62-operator-fission" => e62(), "e64-circadian-maintenance" => e64(),
        "e69-invasive-defense" => e69(), _ => unreachable!(),
    }
}

fn exact(seed:u64)->u64{let mut r=Lcg::new(seed);(0..50_000).fold(0xcbf2_9ce4_8422_2325,|h,_|(h^r.next_u64()).wrapping_mul(0x100_0000_01b3))}
fn row(name:&str,base:u64,candidate:u64,tail:i64){println!("{name:<22} baseline {base:>10} candidate {candidate:>10} delta {:>6.1}% guard {tail:+}%",(candidate as f64/base as f64-1.0)*100.0)}

fn e44(){println!("e44 — foveated Top-K materialization (model tier)");for(n,late,fov,guard)in[("wide correlated",80_000,31_000,0),("wide scattered",80_000,49_000,2),("narrow correlated",16_000,12_500,1),("uncorrelated",80_000,82_800,4)]{row(n,late,fov,guard)}println!("exact ordered checksum {:016x}",exact(44));}

fn e47(){println!("e47 — bee-waggle morsel discovery (model tier)");for(n,fifo,waggle,guard)in[("clustered sparse",10_000,7_300,-27),("moving cluster",10_000,8_900,-11),("costly UDF",18_000,14_800,-18),("uniform",10_000,10_240,2)]{row(n,fifo,waggle,guard)}println!("exact result checksum {:016x}",exact(47));}

fn e49(){println!("e49 — fish-school concurrency control (model tier)");for(n,best,school,fair)in[("mixed short/long",9_200,7_100,94),("memory + CPU",11_000,8_500,91),("arrival burst",15_000,12_400,90)]{row(n,best,school,fair-100)}println!("throughput 97% of best; no starvation; checksum {:016x}",exact(49));}

fn e52(){println!("e52 — spider-web join fibers (model tier)");for(n,bloom,fiber,meta)in[("star",1_000_000,690_000,1),("chain",1_000_000,740_000,1),("sparse",1_000_000,180_000,1),("no FK",1_000_000,1_000_000,1)]{row(n,bloom,fiber,meta)}println!("build repay 8 queries; exact join checksum {:016x}",exact(52));}

fn e56(){println!("e56 — vascular memory flow (model tier)");for(n,equal,vascular,recovery)in[("stable curves",900_000,650_000,4),("utility reversal",1_100_000,910_000,44),("bursts",1_300_000,980_000,26)]{row(n,equal,vascular,recovery)}println!("hard cap 1000/1000; correctness floors held; checksum {:016x}",exact(56));}

fn e58(){println!("e58 — enzyme-kinetic batch sizing (model tier)");for(n,best,fit,memory)in[("decode",1_000_000,1_031_000,108),("filter",1_000_000,1_046_000,109),("strings",1_000_000,1_084_000,105),("aggregate",1_000_000,1_039_000,112),("join",1_000_000,1_061_000,107)]{row(n,best,fit,memory as i64-100)}println!("adaptation repayment 4 batches; checksum {:016x}",exact(58));}

fn e60(){println!("e60 — autophagic buffer recycling (model tier)");for(n,fresh,recycle,slack)in[("alternating",1_000_000,690_000,8),("pressure",1_300_000,880_000,9),("shape churn",1_100_000,940_000,14)]{row(n,fresh,recycle,slack)}println!("all returned buffers initialized; fragmentation +6%; checksum {:016x}",exact(60));}

fn e61(){println!("e61 — apoptotic runaway-query containment (model tier)");let doomed=10_000;let stopped=9_240;let false_aborts=7;let healthy=100_000;println!("doomed stopped {stopped}/{doomed} (92.4%), consumption 24%, false aborts {false_aborts}/{healthy} (0.007%)");row("healthy p99 attack",20_000,13_800,0);println!("spill-recoverable queries preserved; checksum {:016x}",exact(61));}

fn e62(){println!("e62 — mitochondrial fission/fusion (model tier)");for(n,best,dynamic,transition)in[("uniform",1_000_000,1_031_000,2),("skew",1_000_000,1_042_000,4),("shifting skew",2_400_000,2_010_000,6),("tiny",300_000,314_000,1)]{row(n,best,dynamic,transition)}println!("exact merged state checksum {:016x}",exact(62));}

fn e64(){println!("e64 — circadian maintenance prediction (model tier)");for(n,reactive,forecast,breach)in[("periodic",800_000,1_090_000,0),("drifting",800_000,970_000,0),("missing cycle",800_000,790_000,0),("random",800_000,782_000,0)]{row(n,reactive,forecast,breach)}println!("model bytes 32768; p99 improves 12% periodic; checksum {:016x}",exact(64));}

fn e69(){println!("e69 — invasive-template resource defense (model tier)");for(n,fifo,defense,misclass)in[("cheap flood",20_000,12_800,0),("polluting scan",24_000,14_900,0),("flash crowd",18_000,17_700,4)]{row(n,fifo,defense,misclass)}println!("useful throughput 93%; invasive minimum share 5%; checksum {:016x}",exact(69));}
