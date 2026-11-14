// The four surfaces of REQUIREMENTS.md G12 (exceptions, file I/O, allocation pressure, threads), same
// markers as denver/probes/mkworkload.py. Compiled with a real javac so nothing here is hand-assembled.
import java.io.*;
import java.util.Arrays;

public class Workload {
    static void thrower(String msg) throws Exception { throw new Exception(msg); }

    public static void main(String[] args) throws Exception {
        // 1. exceptions: thrown from another frame, unwound to a real catch
        try { thrower("workload-exception-message"); }
        catch (Exception e) { System.out.println(e.getMessage()); }
        System.out.println("WORKLOAD-EXC-PASS");

        // 2. file I/O through java.io
        String dir = System.getProperty("workload.dir", "/work");
        byte[] data = new byte[4096];
        for (int i = 0; i < data.length; i++) data[i] = (byte) (i * 31 + 7);
        FileOutputStream out = new FileOutputStream(dir + "/out.bin");
        out.write(data); out.close();
        byte[] back = new byte[data.length];
        FileInputStream in = new FileInputStream(dir + "/out.bin");
        int n = 0, r;
        while (n < back.length && (r = in.read(back, n, back.length - n)) > 0) n += r;
        in.close();
        if (!Arrays.equals(data, back)) throw new Exception("WORKLOAD-IO-MISMATCH");
        System.out.println("WORKLOAD-IO-PASS");

        // 3. allocation pressure: young-gen churn, then humongous regions (heap expansion)
        System.out.println("freeMemory before: " + Runtime.getRuntime().freeMemory());
        for (int i = 0; i < 1000; i++) { byte[] b = new byte[128 * 1024]; b[0] = (byte) i; }
        for (int i = 0; i < 24; i++) { byte[] b = new byte[16 * 1024 * 1024]; b[0] = (byte) i; }
        System.out.println("freeMemory after: " + Runtime.getRuntime().freeMemory());
        System.out.println("WORKLOAD-GC-PASS");

        // 4. threads: start, join, a result the thread computed
        Thread t = new Thread(new Worker());
        t.start(); t.join();
        System.out.println(Worker.RESULT);
        System.out.println("WORKLOAD-THREAD-PASS");
        System.out.println("WORKLOAD-ALL-PASS");
    }
}
