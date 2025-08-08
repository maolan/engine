use maolan_engine::init;

fn main() {
    println!("before init");
    let client = init();
    println!("before add");
    client.add();
    println!("before quit");
    client.quit();
    println!("end");
}
