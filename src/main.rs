use {
    futures::{
        future::{BoxFuture, FutureExt},
        task::{waker_ref, ArcWake},
    },
    std::{
        future::Future,
        sync::mpsc::{sync_channel, Receiver, SyncSender}, // Menggunakan sync_channel dari std
        sync::{Arc, Mutex},
        task::{Context, Poll},
        time::Duration,
        thread,
    },
};

/// Task executor yang menerima task dari channel dan menjalankannya.
struct Executor {
    ready_queue: Receiver<Arc<Task>>,
}

/// 'Spawner' men-spawn future baru ke task channel.
#[derive(Clone)]
struct Spawner {
    task_sender: SyncSender<Arc<Task>>,
}

/// Sebuah future yang dapat menjadwalkan dirinya kembali untuk di-poll oleh 'Executor'.
struct Task {
    /// Future yang sedang berjalan yang harus didorong hingga selesai.
    /// 'Mutex' di sini diperlukan karena Rust tidak cukup pintar untuk mengetahui
    /// bahwa 'future' hanya dimutasi dari satu thread dalam executor sederhana ini.
    /// Executor produksi mungkin menggunakan 'UnsafeCell'.
    future: Mutex<Option<BoxFuture<'static, ()>>>,

    /// Handle untuk menempatkan task itu sendiri kembali ke antrian task.
    task_sender: SyncSender<Arc<Task>>,
}

fn new_executor_and_spawner() -> (Executor, Spawner) {
    // Jumlah maksimum task yang diizinkan dalam antrian channel.
    // Ini hanya untuk membuat 'sync_channel' "senang" dan tidak akan ada
    // di executor sebenarnya.
    const MAX_QUEUED_TASKS: usize = 10_000;
    let (task_sender, ready_queue) = sync_channel(MAX_QUEUED_TASKS);
    (Executor { ready_queue }, Spawner { task_sender })
}

impl Spawner {
    fn spawn(&self, future: impl Future<Output = ()> + 'static + Send) {
        let future = future.boxed();
        let task = Arc::new(Task {
            future: Mutex::new(Some(future)),
            task_sender: self.task_sender.clone(),
        });
        self.task_sender.send(task).expect("terlalu banyak task diantrikan");
    }
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        // Implementasikan 'wake' dengan mengirim task ini kembali ke task channel
        // sehingga akan di-poll lagi oleh executor.
        let cloned = arc_self.clone();
        arc_self
            .task_sender
            .send(cloned)
            .expect("terlalu banyak task diantrikan saat wake");
    }
}

impl Executor {
    fn run(&self) {
        while let Ok(task) = self.ready_queue.recv() {
            // Ambil future, dan jika belum selesai (masih Some),
            // poll untuk mencoba menyelesaikannya.
            let mut future_slot = task.future.lock().unwrap();
            if let Some(mut future) = future_slot.take() {
                // Buat 'LocalWaker' dari task itu sendiri.
                let waker = waker_ref(&task);
                let context = &mut Context::from_waker(&waker);
                // BoxFuture<T> adalah alias tipe untuk Pin<Box<dyn Future<Output = T> + Send + 'static>>
                // Kita bisa mendapatkan Pin<&mut dyn Future + Send + 'static>
                // darinya dengan memanggil metode 'as_mut()'.
                if future.as_mut().poll(context).is_pending() {
                    // Kita belum selesai memproses future, jadi masukkan kembali
                    // ke dalam task-nya untuk dijalankan lagi di masa mendatang.
                    *future_slot = Some(future);
                }
            }
        }
    }
}

struct TimerFuture {
    shared_state: Arc<Mutex<SharedState>>,
}

/// State bersama antara TimerFuture dan thread yang menunggu.
struct SharedState {
    /// Apakah sleep sudah selesai.
    completed: bool,

    /// Waker untuk task tempat TimerFuture sedang di-poll.
    waker: Option<std::task::Waker>,
}

impl Future for TimerFuture {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut shared_state = self.shared_state.lock().unwrap();
        if shared_state.completed {
            Poll::Ready(())
        } else {
            // Atur waker sehingga thread tahu bagaimana cara membangunkan task saat timer selesai.
            shared_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl TimerFuture {
    /// Buat TimerFuture baru yang akan selesai setelah 'duration'.
    pub fn new(duration: Duration) -> Self {
        let shared_state = Arc::new(Mutex::new(SharedState {
            completed: false,
            waker: None,
        }));

        // Spawn thread baru.
        let thread_shared_state = shared_state.clone();
        thread::spawn(move || {
            thread::sleep(duration);
            let mut shared_state = thread_shared_state.lock().unwrap();
            // Sinyalkan bahwa timer telah selesai dan bangunkan waker terakhir yang terdaftar
            // di poll.
            shared_state.completed = true;
            if let Some(waker) = shared_state.waker.take() {
                waker.wake()
            }
        });

        TimerFuture { shared_state }
    }
}

fn main() {
    let (executor, spawner) = new_executor_and_spawner();

    // Spawn sebuah task untuk mencetak sebelum dan sesudah menunggu timer.
    spawner.spawn(async {
        println!("Fayyed: howdy!");
        // Tunggu TimerFuture selesai setelah dua detik.
        TimerFuture::new(Duration::new(2, 0)).await;
        println!("Fayyed: done!");
    });

    // Drop spawner agar executor tahu bahwa ia telah selesai dan tidak akan
    // menerima task masuk lagi untuk dijalankan.
    // Ini penting agar loop `recv()` di `executor.run()` bisa berakhir.
    drop(spawner);

    // Jalankan executor sampai antrian task kosong.
    executor.run();
}