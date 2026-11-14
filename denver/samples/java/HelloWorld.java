public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello, world - compiled and run on Akuma/denver!");
        System.out.println("java.version = " + System.getProperty("java.version"));
        System.out.println("os.arch      = " + System.getProperty("os.arch"));
        System.out.println("cpus         = " + Runtime.getRuntime().availableProcessors());
        System.out.println("max heap MiB = " + Runtime.getRuntime().maxMemory() / (1024 * 1024));
    }
}
